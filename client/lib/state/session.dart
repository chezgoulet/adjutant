/// Session, the offline read cache, and the queued-write list.
///
/// Offline-first is not a feature flag here — scouts are in the woods, and a
/// screen that shows the last-known truth beats a screen that shows a spinner.
/// Every list read is cached on success and served from cache when the network
/// is gone, with `isStale` telling the UI to say so. Every write goes through
/// [mutate]: it is sent now when the server can be reached, queued when it
/// cannot and the server would treat a repeat as the same act, and refused to
/// the caller's face when it would not (see [Outbox] for why that split is the
/// honest one).
///
/// **The cache's rule, stated once.** There is no TTL. A cached read is served
/// *only* when the server cannot be reached — never in preference to a live
/// answer — so age alone can never make stale data look live: a screen showing
/// cache is showing the [OfflineBanner] with the hour the data is from. What
/// invalidates the cache is the end of the session: [signOut] drops every
/// `cache.*` key, because a roster that outlives the session is one member's
/// troop shown to the next. The queue is dropped with it, for the same reason.
library;

import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/api_client.dart';
import 'outbox.dart';

class Cached<T> {
  const Cached(this.value, {required this.isStale, this.cachedAt});

  final T value;

  /// True when this came from the cache because the network was unreachable.
  final bool isStale;
  final DateTime? cachedAt;
}

/// Raised when a mutating call cannot be made now and must not be queued.
///
/// The distinction this carries is the whole reason it exists: being offline is
/// not a reason to lose a write, but neither is it a reason to let the client
/// invent one. A write whose replay the server would double-apply — a second
/// order, a second Checkout session, a second draw — is refused here, with the
/// reason in words, rather than queued into a promise the client cannot keep.
class OfflineWriteRefused implements Exception {
  OfflineWriteRefused(this.message);

  final String message;

  @override
  String toString() => message;
}

class SessionState extends ChangeNotifier {
  SessionState({ApiClient? client, Outbox? outbox})
      : api = client ?? ApiClient(baseUrl: defaultBaseUrl),
        outbox = outbox ?? Outbox() {
    // The counts the shell shows are the ones on disk, read once at
    // construction: a process that starts while a write is still owed must not
    // look like it has nothing to do.
    unawaited(refreshOutbox());
  }

  static const _tokenKey = 'adjutant.token';
  static const _baseUrlKey = 'adjutant.baseUrl';

  /// The address the app holds before the user has supplied one.
  ///
  /// A debug build gets a developer's own machine, which is genuinely useful for
  /// `flutter run`. A release build gets **nothing**, because there is no honest
  /// default: `localhost` on a phone is the phone, so a pre-filled value is
  /// always wrong — and, worse, plausible, which is how a user ends up debugging
  /// a network they were never talking to. The field starts empty instead, and
  /// the validator says why it is required.
  static final String defaultBaseUrl =
      kDebugMode ? 'http://localhost:8080' : '';

  final ApiClient api;

  /// The durable queued-write list.
  final Outbox outbox;

  Map<String, dynamic>? user;
  bool booting = true;
  bool offline = false;

  /// The writes still owed to the server, oldest first, and the ones it would
  /// not take — both read back from storage, so the UI is showing what is
  /// actually queued rather than what this process remembers queueing.
  List<OutboxEntry> pendingWrites = const [];
  List<RefusedWrite> refusedWrites = const [];

  /// True while a replay pass is in flight, so a read that lands mid-pass does
  /// not start a second one over the same entries.
  bool _syncing = false;

  int get pendingWriteCount => pendingWrites.length;
  int get refusedWriteCount => refusedWrites.length;

  bool get isAuthenticated => user != null;
  String get displayName =>
      (user?['display_name'] as String?)?.trim().isNotEmpty == true
          ? user!['display_name'] as String
          : (user?['username'] as String? ?? '');
  List<String> get roles =>
      (user?['roles'] as List?)?.map((e) => e.toString()).toList() ?? const [];

  /// Restore a previous session on launch. A network failure here does *not*
  /// sign the user out — the token is still good; we are just offline.
  Future<void> boot() async {
    final prefs = await SharedPreferences.getInstance();
    final savedBase = prefs.getString(_baseUrlKey);
    if (savedBase != null && savedBase.isNotEmpty) {
      // Apply the address the user signed in with, before anything below asks
      // the server for anything. This is the whole point of having saved it:
      // without it the app relaunched onto the built-in default, asked *that*
      // address for the session, failed, and showed the cached user as
      // "offline" — so every launch after the first was broken, and nothing in
      // the UI said why.
      api.setBaseUrl(savedBase);
    }
    final token = prefs.getString(_tokenKey);
    if (token != null && token.isNotEmpty) {
      api.setToken(token);
      try {
        user = await api.me();
        offline = false;
        // The server answered, so this is the moment to hand it anything the
        // last session left owing. Before `booting = false`, so the shell's
        // first frame already shows the queue it actually has.
        await syncOutbox();
      } on OfflineException {
        // Keep the token; we cannot confirm the session but we are not wrong to
        // hold it. The UI shows the offline banner.
        offline = true;
        user = _cachedUser(prefs);
      } on ApiException catch (e) {
        if (e.isUnauthorized) {
          await _clearToken(prefs);
          user = null;
        } else {
          offline = true;
          user = _cachedUser(prefs);
        }
      }
    }
    booting = false;
    notifyListeners();
  }

  Map<String, dynamic>? _cachedUser(SharedPreferences prefs) {
    final raw = prefs.getString('adjutant.user');
    if (raw == null) return null;
    try {
      return jsonDecode(raw) as Map<String, dynamic>;
    } on FormatException {
      return null;
    }
  }

  Future<void> signIn({
    required String baseUrl,
    required String username,
    required String password,
  }) async {
    api.setToken(null);
    // Point the client at the address the user typed *before* logging in.
    // Saving it was never enough on its own: `api` kept the built-in default, so
    // the sign-in request itself went to the default and the Server field had no
    // effect whatsoever — it accepted anything and the app talked to
    // `localhost` regardless.
    api.setBaseUrl(baseUrl);
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString(_baseUrlKey, baseUrl);
    final token = await api.login(username: username, password: password);
    user = await api.me();
    await prefs.setString(_tokenKey, token);
    await prefs.setString('adjutant.user', jsonEncode(user));
    offline = false;
    // Signing in is the other moment the server is known reachable.
    await syncOutbox();
    notifyListeners();
  }

  /// Sign out, and end the session's claim on the device.
  ///
  /// The token goes, and so do the read cache and the queued-write list. Both
  /// are facts about *this* person's troop: a cached roster that outlives the
  /// session is the next member's app showing the last member's troop, and a
  /// queue that outlives it would replay one person's change as whoever signs
  /// in next. If writes are still owed, the shell says so before calling this —
  /// they are discarded here only after the person has been told.
  Future<void> signOut() async {
    await api.logout();
    final prefs = await SharedPreferences.getInstance();
    await _clearToken(prefs);
    await _clearCache(prefs);
    await outbox.clear();
    await refreshOutbox();
    user = null;
    notifyListeners();
  }

  /// Drop every cached read. The explicit invalidation rule this cache has
  /// (see the library doc): no TTL, and the cache dies with the session.
  Future<void> _clearCache(SharedPreferences prefs) async {
    for (final key in prefs.getKeys().toList()) {
      if (key.startsWith('cache.')) await prefs.remove(key);
    }
  }

  Future<void> _clearToken(SharedPreferences prefs) async {
    await prefs.remove(_tokenKey);
    await prefs.remove('adjutant.user');
  }

  // --- offline-cached reads ------------------------------------------------

  /// Read a collection through the cache: fresh on success, cached on a network
  /// failure. An authorisation failure is *not* swallowed into a stale read —
  /// it propagates so the shell can sign the user out.
  Future<Cached<List<Map<String, dynamic>>>> cachedList(
    String key,
    Future<List<Map<String, dynamic>>> Function() fetch,
  ) async {
    final prefs = await SharedPreferences.getInstance();
    try {
      final fresh = await fetch();
      await prefs.setString('cache.$key', jsonEncode(fresh));
      await prefs.setString('cache.$key.at', DateTime.now().toIso8601String());
      offline = false;
      // A read that landed proves the server is reachable, which is the whole
      // trigger for "syncs when online": the queue is drained here rather than
      // by a timer nobody asked for.
      await _syncIfPending();
      return Cached(fresh, isStale: false, cachedAt: DateTime.now());
    } on OfflineException {
      offline = true;
      return Cached(_readCache(prefs, key), isStale: true, cachedAt: _readCacheAt(prefs, key));
    }
  }

  /// Read a single object through the cache, the same contract as
  /// [cachedList]: fresh on success, last-known on a network failure, and an
  /// authorisation refusal propagated rather than folded into a stale read.
  Future<Cached<Map<String, dynamic>>> cachedMap(
    String key,
    Future<Map<String, dynamic>> Function() fetch,
  ) async {
    final prefs = await SharedPreferences.getInstance();
    try {
      final fresh = await fetch();
      await prefs.setString('cache.$key', jsonEncode(fresh));
      await prefs.setString('cache.$key.at', DateTime.now().toIso8601String());
      offline = false;
      await _syncIfPending();
      return Cached(fresh, isStale: false, cachedAt: DateTime.now());
    } on OfflineException {
      offline = true;
      return Cached(_readCachedMap(prefs, key),
          isStale: true, cachedAt: _readCacheAt(prefs, key));
    }
  }

  Map<String, dynamic> _readCachedMap(SharedPreferences prefs, String key) {
    final raw = prefs.getString('cache.$key');
    if (raw == null) return const {};
    try {
      final decoded = jsonDecode(raw);
      if (decoded is Map) return Map<String, dynamic>.from(decoded);
    } on FormatException {
      // A corrupt cache is not worth crashing over; nothing is honest.
    }
    return const {};
  }

  // --- the queued-write list ------------------------------------------------

  /// Re-read the queue and the refused list from storage into memory, so the UI
  /// renders what is actually recorded rather than what this process remembers.
  Future<void> refreshOutbox() async {
    try {
      pendingWrites = await outbox.pending();
      refusedWrites = await outbox.refused();
    } on Object catch (e) {
      // No durable storage on this device. The queue is then empty and nothing
      // can be recorded in it — a state the write path reports honestly
      // (`_mutate` refuses to claim a write was queued when it was not), rather
      // than a reason to crash the shell.
      debugPrint('cannot read the queued-write list: $e');
      pendingWrites = const [];
      refusedWrites = const [];
    }
    notifyListeners();
  }

  /// Replay what is owed, oldest first. Safe to call at any time — a pass
  /// already in flight is not started twice.
  ///
  /// This is what "syncs when online" means in code: it is called when the
  /// server has just proved reachable (a read that landed, a sign-in, a write
  /// that went through) and by an explicit retry from the UI. There is no timer
  /// and no background worker: nothing here runs that the person did not cause.
  Future<FlushReport> syncOutbox() async {
    if (_syncing) return const FlushReport();
    _syncing = true;
    try {
      final report = await outbox.flush(
        send: (entry) => api.send(entry.method, entry.path, body: entry.body),
      );
      if (report.sent > 0) offline = false;
      await refreshOutbox();
      return report;
    } finally {
      _syncing = false;
    }
  }

  /// Cheap gate for the read path: nothing owed, nothing to do.
  Future<void> _syncIfPending() async {
    if (_syncing) return;
    if ((await outbox.pending()).isEmpty) return;
    await syncOutbox();
  }

  /// Forget the writes the server refused, once their reasons have been read.
  Future<void> clearRefusedWrites() async {
    await outbox.clearRefused();
    await refreshOutbox();
  }

  /// One mutating call, through the queue's rules.
  ///
  /// [online] is the call the client already knew how to make; [method], [path]
  /// and [body] are the same request written down, because that is what the
  /// queue has to store to replay it later (the tests assert the two agree).
  /// [replaySafe] is a claim about the *server's* rule for the route, not a
  /// guess about this one: true only where the server treats a repeat as the
  /// same act (see [Outbox]'s doc for the list and the reasoning). Where it is
  /// false the write is refused in words instead of queued — a queued write the
  /// server would double-apply is worse than a write that never left.
  Future<Map<String, dynamic>> _mutate({
    required String method,
    required String path,
    Object? body,
    required Future<Map<String, dynamic>> Function() online,
    required bool replaySafe,
    String refusal = '',
  }) async {
    try {
      final value = await online();
      offline = false;
      // The server answered, so it is reachable: this is the moment to hand it
      // what was owed.
      await _syncIfPending();
      return value;
    } on OfflineException catch (e) {
      if (!replaySafe) {
        throw OfflineWriteRefused(refusal);
      }
      offline = true;
      OutboxEntry entry;
      try {
        entry = await outbox.enqueue(OutboxEntry(
          key: Outbox.keyFor(method, path, body),
          method: method,
          path: path,
          body: body,
          queuedAt: DateTime.now(),
        ));
      } on Object catch (queueFailure) {
        // The write could not be recorded either. Claiming it is queued would
        // be the one lie this list exists to prevent, so the offline failure is
        // what surfaces: the change did not leave, and it is not owed.
        debugPrint('could not queue $method $path: $queueFailure');
        throw e;
      }
      await refreshOutbox();
      debugPrint('queued for the server: ${entry.method} ${entry.path} '
          '(${entry.key}; ${e.cause ?? 'server unreachable'})');
      // `queued` is the tell for the caller: there is no server answer because
      // there was no server. No other route returns this key, so nothing the
      // server sends can be mistaken for it.
      return {'queued': true, 'outbox_key': entry.key};
    }
  }

  /// Record the caller's own receipt on an announcement. Queued when offline:
  /// the server writes no second receipt for a repeat and says `already_read`,
  /// so a replay after a crash is the same act, not a second one.
  Future<Map<String, dynamic>> markAnnouncementRead(String id) => _mutate(
        method: 'POST',
        path: '/api/announcements/announcement/$id/read',
        body: const {'via': 'flutter'},
        online: () => api.markAnnouncementRead(id),
        replaySafe: true,
      );

  /// Clear the caller's own receipt. Queued when offline: forgetting twice is
  /// not an error, which is the server's own sentence about this route.
  Future<Map<String, dynamic>> markAnnouncementUnread(String id) => _mutate(
        method: 'POST',
        path: '/api/announcements/announcement/$id/unread',
        online: () => api.markAnnouncementUnread(id),
        replaySafe: true,
      );

  /// Report your own sliding-scale tier. Queued when offline: the route is a
  /// setter for one member and one year, so replaying it after a crash sets the
  /// same tier again rather than adding a second one — and two reports made
  /// offline replay in the order they were made, so the last one is the one
  /// that stands.
  Future<Map<String, dynamic>> selfReportDues({
    required String tier,
    int? fiscalYear,
    String? note,
  }) {
    final body = <String, Object>{
      'tier': tier,
      'fiscal_year': ?fiscalYear,
      if (note != null && note.trim().isNotEmpty) 'note': note.trim(),
    };
    return _mutate(
      method: 'POST',
      path: '/api/finance/dues/self-report',
      body: body,
      online: () => api.selfReportDues(
          tier: tier, fiscalYear: fiscalYear, note: note),
      replaySafe: true,
    );
  }

  /// Enable or disable a plugin. Queued when offline: the core's own rule is
  /// that enabling an enabled plugin and disabling a disabled one are no-ops,
  /// so the verb replays as the same act.
  ///
  /// Returns whether the server took it: false means it is queued.
  Future<bool> setPluginEnabled(String id, bool enabled) async {
    final verb = enabled ? 'enable' : 'disable';
    final answer = await _mutate(
      method: 'POST',
      path: '/api/plugins/${Uri.encodeComponent(id)}/$verb',
      online: () async {
        await api.setPluginEnabled(id, enabled);
        return const <String, dynamic>{};
      },
      replaySafe: true,
    );
    return answer['queued'] != true;
  }

  /// Place a shop order.
  ///
  /// **Never queued — refused when the server cannot be reached, with the
  /// reason.** `POST /api/store/order` *appends* an order; the server has no
  /// caller-supplied key on that route with which a second one could be told
  /// from the first, so a replay would buy the patch twice and charge for it.
  /// There is no honest queue for that: the person is told it did not go, and
  /// the order stays unplaced until they place it. The same rule covers every
  /// appended write on this client — opening a Checkout session, completing an
  /// order, booking a draw, checking equipment out — which is why only the
  /// setter-shaped writes go through the queue.
  Future<Map<String, dynamic>> placeStoreOrder({
    required List<Map<String, Object>> lines,
    String? tier,
    String? memberId,
    String? note,
  }) {
    final body = <String, Object>{
      'lines': lines,
      'tier': ?tier,
      'member_id': ?memberId,
      if (note != null && note.trim().isNotEmpty) 'note': note.trim(),
    };
    return _mutate(
      method: 'POST',
      path: '/api/store/order',
      body: body,
      online: () => api.placeStoreOrder(
          lines: lines, tier: tier, memberId: memberId, note: note),
      replaySafe: false,
      refusal: 'You are offline, and an order cannot be queued: the server '
          'would create a second one when it came back. Nothing was ordered — '
          'place it again when you have a connection.',
    );
  }

  // --- the announcement badge ----------------------------------------------

  /// The last badge the server reported, held here because the shell shows it
  /// on a destination and the inbox screen is what changes it. Null until the
  /// server has ever answered: a count this client invented would be a lie, and
  /// no count is a truthful "I do not know yet".
  Map<String, dynamic>? announcementBadge;

  int get announcementUnread =>
      (announcementBadge?['unread'] as num?)?.toInt() ?? 0;

  /// Whether any *unread* announcement is urgent — the badge that must not be
  /// mistaken for routine.
  bool get announcementUrgent => announcementBadge?['has_urgent'] == true;

  /// Load the badge into the session for the shell to render.
  ///
  /// A refusal is not an offline read and is not swallowed on the screen's
  /// behalf; here it simply leaves the badge empty, because a caller without
  /// `announcements:read` genuinely has nothing counted and the inbox screen is
  /// where the refusal is stated in words.
  Future<void> refreshAnnouncementBadge() async {
    try {
      final cached =
          await cachedMap('announcement.badge', api.unreadAnnouncements);
      setAnnouncementBadge(cached.value);
    } on ApiException {
      // Nothing to badge. The destination stays, because hiding it would be a
      // client-side guess at the caller's permissions.
    }
  }

  /// Adopt a badge the server just returned. Every read/unread response and the
  /// list payload carry the fresh count, so the shell's badge updates from the
  /// server's answer instead of a number this client increments itself.
  void setAnnouncementBadge(Map<String, dynamic>? badge) {
    if (badge == null || badge.isEmpty) return;
    announcementBadge = badge;
    notifyListeners();
  }

  List<Map<String, dynamic>> _readCache(SharedPreferences prefs, String key) {
    final raw = prefs.getString('cache.$key');
    if (raw == null) return const [];
    try {
      final decoded = jsonDecode(raw);
      if (decoded is List) {
        return decoded.whereType<Map>().map((e) => Map<String, dynamic>.from(e)).toList();
      }
    } on FormatException {
      // A corrupt cache is not worth crashing over; an empty list is honest.
    }
    return const [];
  }

  DateTime? _readCacheAt(SharedPreferences prefs, String key) {
    final raw = prefs.getString('cache.$key.at');
    return raw == null ? null : DateTime.tryParse(raw);
  }
}
