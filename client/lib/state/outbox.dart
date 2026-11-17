/// The queued-write list — the half of offline mode that is not a cache.
///
/// A read can be served from the cache; a *write* cannot. What a scout does in
/// the woods — records a receipt on a notice, reports their dues tier, switches
/// a plugin off for the season — has to survive until the server is reachable,
/// and it has to survive a restart in between. That is this list.
///
/// Owner decision, 2026-09-27 (`docs/plugin-roadmap.md` §6.2): a plain durable
/// queue over `shared_preferences`, no schema on the device. There is no
/// migration story here and none is needed: every entry is a JSON object in a
/// list, and the list is rewritten after each entry resolves.
///
/// **What replay guarantees, and what it does not.** Replay is in order, oldest
/// first, and stops at the first sign that the server is gone again — an entry
/// behind an unsent one is never sent ahead of it. Delivery is therefore
/// *at least once*, not exactly once: an app killed between the server's 2xx and
/// the list rewrite replays that one entry. That is why only writes the server
/// itself treats as a repeat of the same act are queued at all (see
/// `SessionState.mutate`'s `replaySafe`): marking a receipt read twice writes no
/// second receipt, forgetting twice is not an error, a dues self-report is a
/// setter, enabling an enabled plugin is a no-op. A write the server would
/// double-apply — placing an order, opening a Checkout session, booking a draw —
/// is refused at the call site instead of being queued, and the person is told
/// so. Nothing is invented server-side: no header, no field, no route.
library;

import 'dart:convert';

import 'package:shared_preferences/shared_preferences.dart';

import '../api/api_client.dart';

/// One recorded write, waiting for the server.
class OutboxEntry {
  const OutboxEntry({
    required this.key,
    required this.owner,
    required this.method,
    required this.path,
    this.body,
    required this.queuedAt,
    this.attempts = 0,
  });

  /// The entry's idempotency key: a stable digest of *what the write is* and
  /// *whose write it is* (owner, method, path and body), not a random id.
  ///
  /// Two consequences, both of them the point. The same write queued twice —
  /// a double tap, a screen rebuilt — lands as one entry, because the key is
  /// derived and the second enqueue finds the first. And an entry replayed
  /// after a crash carries the key it was queued with, so the queue's own
  /// record of "this one act" is never two records.
  ///
  /// The owner is part of the digest because a request digest alone is
  /// identical for every member who makes the same request (#166): identity A's
  /// queued dues self-report would have found identity B's entry, or replayed
  /// under B's token. The identity is not a secret and is not sent to the
  /// server; it names whose queue this entry belongs to on this device.
  ///
  /// It is deliberately **not** sent to the server. No route on the client's
  /// writing surface accepts a caller-supplied idempotency key, and inventing
  /// one — an `Idempotency-Key` header the core ignores — would be a promise
  /// this client cannot keep. Duplicate suppression is delegated to the rules
  /// the server already has, which is what makes `replaySafe` a decision taken
  /// per call site rather than a claim made here.
  final String key;

  /// The authenticated member id that made this write. An entry is only ever
  /// replayed while that member is the current user.
  final String owner;

  final String method;
  final String path;
  final Object? body;
  final DateTime queuedAt;

  /// How many times the server has answered 5xx to this entry. A 5xx is "try
  /// again", so the entry keeps its place and the count climbs; past
  /// [Outbox.maxAttempts] it is parked as refused rather than retried forever.
  final int attempts;

  OutboxEntry withAttempt() => OutboxEntry(
        key: key,
        owner: owner,
        method: method,
        path: path,
        body: body,
        queuedAt: queuedAt,
        attempts: attempts + 1,
      );

  Map<String, dynamic> toJson() => {
        'key': key,
        'owner': owner,
        'method': method,
        'path': path,
        if (body != null) 'body': body,
        'queued_at': queuedAt.toIso8601String(),
        'attempts': attempts,
      };

  /// Read one entry back, or null when it cannot be read. A corrupt entry is
  /// skipped rather than crashing the queue — but see [Outbox.flush]: it is
  /// reported, not swallowed.
  ///
  /// An entry with no `owner` predates identity-keyed queues. It cannot be
  /// attributed to a member, so it is refused rather than replayed as whoever
  /// is signed in now — the exact defect #166 records. It is dropped from the
  /// stored list the next time the queue is rewritten.
  static OutboxEntry? fromJson(Object? raw) {
    if (raw is! Map) return null;
    final key = raw['key'];
    final owner = raw['owner'];
    final method = raw['method'];
    final path = raw['path'];
    if (key is! String || owner is! String || owner.isEmpty) return null;
    if (method is! String || path is! String) return null;
    return OutboxEntry(
      key: key,
      owner: owner,
      method: method,
      path: path,
      body: raw['body'],
      queuedAt: DateTime.tryParse('${raw['queued_at']}') ?? DateTime.now(),
      attempts: (raw['attempts'] as num?)?.toInt() ?? 0,
    );
  }
}

/// A queued write the server will not take: it has been answered, and the
/// answer was a refusal (or the retries ran out).
///
/// It is kept, with the server's own words, so the person can read what
/// happened and decide — it is never dropped quietly, and it never holds the
/// queue behind it.
class RefusedWrite {
  const RefusedWrite({
    required this.entry,
    required this.statusCode,
    required this.message,
    required this.refusedAt,
    this.attempts = 1,
  });

  final OutboxEntry entry;

  /// The status the server answered, or 0 when the failure was the client's
  /// own (a decode fault, a bug) rather than a refusal.
  final int statusCode;

  /// The server's sentence, kept verbatim, or a description of the local fault.
  final String message;

  final DateTime refusedAt;
  final int attempts;

  String get summary => statusCode == 0
      ? '${entry.method} ${entry.path} — $message'
      : '${entry.method} ${entry.path} — $statusCode: $message';

  Map<String, dynamic> toJson() => {
        'entry': entry.toJson(),
        'status': statusCode,
        'message': message,
        'refused_at': refusedAt.toIso8601String(),
        'attempts': attempts,
      };

  static RefusedWrite? fromJson(Object? raw) {
    if (raw is! Map) return null;
    final entry = OutboxEntry.fromJson(raw['entry']);
    if (entry == null) return null;
    return RefusedWrite(
      entry: entry,
      statusCode: (raw['status'] as num?)?.toInt() ?? 0,
      message: '${raw['message'] ?? 'refused'}',
      refusedAt: DateTime.tryParse('${raw['refused_at']}') ?? DateTime.now(),
      attempts: (raw['attempts'] as num?)?.toInt() ?? 1,
    );
  }
}

/// What one pass of [Outbox.flush] did.
class FlushReport {
  const FlushReport({
    this.sent = 0,
    this.refused = 0,
    this.remaining = 0,
    this.unreachable = false,
  });

  /// Entries the server accepted (2xx) and which have left the queue.
  final int sent;

  /// Entries parked as refused: a 4xx, or a 5xx past [Outbox.maxAttempts].
  final int refused;

  /// Entries still queued, in order, after this pass.
  final int remaining;

  /// The pass stopped on a transport failure: the server is unreachable again,
  /// and the queue was left intact from that entry onward.
  final bool unreachable;

  bool get didSomething => sent > 0 || refused > 0;
}

/// The durable queue itself.
///
/// Every method reads and writes `shared_preferences`, so an entry survives the
/// app being killed. The queue is two keys: the list of what is owed, and the
/// list of what the server would not take.
class Outbox {
  Outbox({DateTime Function()? clock}) : _clock = clock ?? DateTime.now;

  /// The list of writes still owed, oldest first.
  static const String queueKey = 'outbox.queue';

  /// The list of writes the server refused, newest last.
  static const String refusedKey = 'outbox.refused';

  /// How many 5xx answers one entry is allowed before it is parked as refused.
  /// A route that is broken rather than busy must not block the queue forever,
  /// and three is enough to rule out a single bad moment.
  static const int maxAttempts = 3;

  final DateTime Function() _clock;

  /// The idempotency key for one write: a stable digest of the owner and the
  /// request (method, path and body), so the same act queued twice by the same
  /// member is one entry ([OutboxEntry.key]) and the same act by two different
  /// members is two.
  ///
  /// FNV-1a over the UTF-8 bytes of the canonical form. It is not a
  /// cryptographic hash and does not need to be: it is a name for a write
  /// inside one device's list, not a security boundary. `String.hashCode` would
  /// not do — Dart does not promise it is stable across runs, and this key has
  /// to mean the same thing after a restart.
  static String keyFor(
    String method,
    String path,
    Object? body, {
    required String owner,
  }) {
    final canonical =
        '$owner $method $path ${body == null ? '' : jsonEncode(body)}';
    var hash = 0x811c9dc5;
    for (final byte in utf8.encode(canonical)) {
      hash ^= byte;
      hash = (hash * 0x01000193) & 0xffffffff;
    }
    return 'outbox-${hash.toRadixString(36)}';
  }

  /// What this member still owes, oldest first.
  ///
  /// [owner] is the authenticated member id. Passing null returns the whole
  /// stored list (used by diagnostics and the queue's own tests); replay always
  /// names an owner.
  Future<List<OutboxEntry>> pending({String? owner}) async {
    final prefs = await SharedPreferences.getInstance();
    return _ownedEntries(_queue(prefs), owner);
  }

  Future<List<RefusedWrite>> refused({String? owner}) async {
    final prefs = await SharedPreferences.getInstance();
    return _ownedRefusals(_refused(prefs), owner);
  }

  /// Record one write. Idempotent by key: a write already owed is not queued
  /// twice, and the entry already there is returned.
  Future<OutboxEntry> enqueue(OutboxEntry entry) async {
    final prefs = await SharedPreferences.getInstance();
    final queue = _queue(prefs);
    for (final existing in queue) {
      if (existing.key == entry.key) return existing;
    }
    queue.add(entry);
    await _writeQueue(prefs, queue);
    return entry;
  }

  /// Replay the queue in order, oldest first.
  ///
  /// [send] performs one entry's request and returns when the server has
  /// answered 2xx; it throws [OfflineException] when the server cannot be
  /// reached and [ApiException] for any non-2xx answer.
  ///
  /// Only entries whose [OutboxEntry.owner] is [owner] are replayed. Another
  /// member's entries are left exactly where they are — not sent, and not
  /// deleted — because this device may be shared and that member's queue is
  /// still theirs. This is the half of #166 that survives a cache clear: an
  /// artefact is refused unless its owner is the current user.
  ///
  /// The loop is deliberate about three things:
  ///
  /// 1. **Order.** An entry is only ever sent after every *owned* entry in front
  ///    of it has left the queue. A transport failure stops the pass where it
  ///    stands; nothing behind it jumps ahead.
  /// 2. **A refusal is not a loss and not a wall.** A 4xx parks that entry in
  ///    the refused list, with the server's words, and the pass carries on with
  ///    the next one. A 5xx keeps the entry in place and counts; past
  ///    [maxAttempts] it is parked too.
  /// 3. **Crash-safety.** The stored queue is rewritten after *each* entry
  ///    resolves, so the worst an interrupted pass can do is replay the one
  ///    entry in flight — never lose the ones behind it, and never lose another
  ///    owner's entries.
  Future<FlushReport> flush({
    required String owner,
    required Future<dynamic> Function(OutboxEntry) send,
  }) async {
    final prefs = await SharedPreferences.getInstance();
    final all = _queue(prefs);
    final mine = all.where((entry) => entry.owner == owner).toList();
    if (mine.isEmpty) return const FlushReport();

    final refused = _refused(prefs);
    // The whole list, owned and foreign alike. Foreign entries are never
    // touched; owned ones leave it as they resolve.
    final remaining = List<OutboxEntry>.of(all);
    var sent = 0;
    var parked = 0;
    var stopped = false;

    for (final entry in mine) {
      if (stopped) break;
      try {
        await send(entry);
        sent++;
        remaining.removeWhere((e) => e.key == entry.key);
      } on OfflineException {
        // The server is gone again. Stop here, keep this entry and everything
        // behind it, in order.
        stopped = true;
      } on ApiException catch (e) {
        if (e.statusCode >= 400 && e.statusCode < 500) {
          // A refusal is an answer: this write will never succeed by waiting,
          // so it is parked with the server's own sentence and the queue moves
          // on. Waiting for a human is not a reason to hold up everything else.
          refused.add(RefusedWrite(
            entry: entry,
            statusCode: e.statusCode,
            message: e.message,
            refusedAt: _clock(),
          ));
          parked++;
          remaining.removeWhere((e) => e.key == entry.key);
        } else {
          final retried = entry.withAttempt();
          if (retried.attempts >= maxAttempts) {
            refused.add(RefusedWrite(
              entry: retried,
              statusCode: e.statusCode,
              message: 'the server answered ${e.statusCode} '
                  '(${e.message}) $maxAttempts times',
              refusedAt: _clock(),
              attempts: retried.attempts,
            ));
            parked++;
            remaining.removeWhere((e) => e.key == entry.key);
          } else {
            // Busy, not broken: this entry keeps its place and is tried again
            // on the next pass, while the writes behind it are not held up by
            // one server hiccup.
            final at = remaining.indexWhere((e) => e.key == entry.key);
            if (at >= 0) remaining[at] = retried;
          }
        }
      } on Object catch (e) {
        // Anything else the client itself threw. The entry is parked rather
        // than lost, and the queue is not held behind a fault this client
        // cannot name.
        refused.add(RefusedWrite(
          entry: entry,
          statusCode: 0,
          message: '$e',
          refusedAt: _clock(),
        ));
        parked++;
        remaining.removeWhere((e) => e.key == entry.key);
      }

      // Rewritten per entry (see the crash-safety note above): what is kept,
      // including every other owner's entry, untouched.
      await _writeQueue(prefs, remaining);
      await _writeRefused(prefs, refused);
    }

    return FlushReport(
      sent: sent,
      refused: parked,
      remaining: remaining.where((entry) => entry.owner == owner).length,
      unreachable: stopped,
    );
  }

  /// Drop the refused list. Used when the person has read it and dismissed it —
  /// the queue is theirs to clear.
  Future<void> clearRefused({String? owner}) async {
    final prefs = await SharedPreferences.getInstance();
    if (owner == null) {
      await prefs.remove(refusedKey);
      return;
    }
    final kept = _refused(prefs).where((write) => write.entry.owner != owner);
    await _writeRefused(prefs, kept.toList());
  }

  /// Drop everything: the queue and the refused list.
  ///
  /// Called when the session ends — by `signOut`, and by a 401 at boot, which
  /// is the same end from the server's side. A queued write belongs to the
  /// session that made it — a queue that outlived the session would replay one
  /// person's change as whoever signs in next. Per-owner filtering already
  /// refuses that replay; clearing is the stronger act, and the one the session
  /// documents.
  Future<void> clear() async {
    final prefs = await SharedPreferences.getInstance();
    await prefs.remove(queueKey);
    await prefs.remove(refusedKey);
  }

  // --- storage --------------------------------------------------------------

  static List<OutboxEntry> _ownedEntries(
    List<OutboxEntry> writes,
    String? owner,
  ) =>
      owner == null
          ? writes
          : writes.where((entry) => entry.owner == owner).toList();

  static List<RefusedWrite> _ownedRefusals(
    List<RefusedWrite> writes,
    String? owner,
  ) =>
      owner == null
          ? writes
          : writes.where((write) => write.entry.owner == owner).toList();

  List<OutboxEntry> _queue(SharedPreferences prefs) {
    final raw = prefs.getStringList(queueKey) ?? const <String>[];
    final out = <OutboxEntry>[];
    for (final line in raw) {
      final entry = _decode(line, OutboxEntry.fromJson);
      if (entry != null) out.add(entry);
    }
    return out;
  }

  List<RefusedWrite> _refused(SharedPreferences prefs) {
    final raw = prefs.getStringList(refusedKey) ?? const <String>[];
    final out = <RefusedWrite>[];
    for (final line in raw) {
      final entry = _decode(line, RefusedWrite.fromJson);
      if (entry != null) out.add(entry);
    }
    return out;
  }

  Future<void> _writeQueue(SharedPreferences prefs, List<OutboxEntry> queue) async {
    if (queue.isEmpty) {
      await prefs.remove(queueKey);
      return;
    }
    await prefs.setStringList(
      queueKey,
      queue.map((e) => jsonEncode(e.toJson())).toList(),
    );
  }

  Future<void> _writeRefused(SharedPreferences prefs, List<RefusedWrite> refused) async {
    if (refused.isEmpty) {
      await prefs.remove(refusedKey);
      return;
    }
    await prefs.setStringList(
      refusedKey,
      refused.map((e) => jsonEncode(e.toJson())).toList(),
    );
  }

  /// One stored line, decoded — or null when it is unreadable.
  static T? _decode<T>(String line, T? Function(Object?) parse) {
    try {
      return parse(jsonDecode(line));
    } on FormatException {
      return null;
    }
  }
}
