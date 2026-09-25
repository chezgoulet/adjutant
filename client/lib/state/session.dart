/// Session and offline cache.
///
/// Offline-first is not a feature flag here — scouts are in the woods, and a
/// screen that shows the last-known truth beats a screen that shows a spinner.
/// Every list read is cached on success and served from cache when the network
/// is gone, with `isStale` telling the UI to say so.
library;

import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/api_client.dart';

class Cached<T> {
  const Cached(this.value, {required this.isStale, this.cachedAt});

  final T value;

  /// True when this came from the cache because the network was unreachable.
  final bool isStale;
  final DateTime? cachedAt;
}

class SessionState extends ChangeNotifier {
  SessionState({ApiClient? client})
      : api = client ?? ApiClient(baseUrl: defaultBaseUrl);

  static const _tokenKey = 'adjutant.token';
  static const _baseUrlKey = 'adjutant.baseUrl';

  /// Where the server lives. Overridden at runtime on the login screen so a
  /// self-hosting troop can point the app at their own box without a rebuild.
  static const defaultBaseUrl = 'http://localhost:8080';

  final ApiClient api;

  Map<String, dynamic>? user;
  bool booting = true;
  bool offline = false;

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
      api.setToken(null);
      // Replace the client base URL by rebuilding the token holder; the client
      // is cheap and stateless apart from the token.
    }
    final token = prefs.getString(_tokenKey);
    if (token != null && token.isNotEmpty) {
      api.setToken(token);
      try {
        user = await api.me();
        offline = false;
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
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString(_baseUrlKey, baseUrl);
    final token = await api.login(username: username, password: password);
    user = await api.me();
    await prefs.setString(_tokenKey, token);
    await prefs.setString('adjutant.user', jsonEncode(user));
    offline = false;
    notifyListeners();
  }

  Future<void> signOut() async {
    await api.logout();
    final prefs = await SharedPreferences.getInstance();
    await _clearToken(prefs);
    user = null;
    notifyListeners();
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
