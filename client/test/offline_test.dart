/// Offline mode, proved: the durable read cache, the queued-write list, replay
/// order, and the two ways a write must not be lost.
///
/// These tests drive the session and the queue the way the screens do — a mock
/// `http.Client` for the wire, `SharedPreferences.setMockInitialValues` for the
/// device's storage — and they assert on what was *sent* and what is *stored*,
/// not on what the client intended.
///
/// The live harness (`live/live_client_test.dart`, run by
/// `scripts/client-live-harness.sh`) is the other half: it drives the app's own
/// `ApiClient` against a real server, and it is why a cache that swallowed a
/// live response would be caught rather than praised.
library;

import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/announcements_screen.dart';
import 'package:adjutant_client/state/outbox.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// A JSON response the way the server sends one: UTF-8 bytes.
http.Response jsonResponse(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

/// One request, as the server would see it.
class SentRequest {
  SentRequest(this.method, this.path, this.body);

  final String method;
  final String path;
  final Object? body;

  /// The request in the queue's recorded shape: method, path, and body as the
  /// entry stores it.
  Map<String, Object?> get recorded => {
        'method': method,
        'path': path,
        'body': body,
      };

  @override
  String toString() => '$method $path';
}

/// A client whose server is unreachable — the offline case, from the wire's
/// point of view.
ApiClient offlineClient({List<SentRequest>? sent}) => ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((request) async {
        sent?.add(SentRequest(
          request.method,
          request.url.path,
          request.body.isEmpty ? null : jsonDecode(request.body),
        ));
        throw http.ClientException('no route to host');
      }),
    );

/// A client whose server answers, recording every request.
ApiClient liveClient({
  required List<SentRequest> sent,
  int status = 200,
  Object body = const {'ok': true},
}) =>
    ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((request) async {
        sent.add(SentRequest(
          request.method,
          request.url.path,
          request.body.isEmpty ? null : jsonDecode(request.body),
        ));
        return jsonResponse(body, status);
      }),
    );

void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  // -------------------------------------------------------------------------
  // The queued-write list
  // -------------------------------------------------------------------------

  group('the queued-write list', () {
    test('records a mutating call made offline, with its body and its key',
        () async {
      final sent = <SentRequest>[];
      final session = SessionState(client: offlineClient(sent: sent));

      final answer = await session.markAnnouncementRead('7');

      // The attempt was made and there was no server to take it...
      expect(sent.single.recorded, {
        'method': 'POST',
        'path': '/api/announcements/announcement/7/read',
        'body': {'via': 'flutter'},
      });
      // ...and it is recorded, with what would have been sent.
      expect(answer['queued'], isTrue);
      expect(session.pendingWriteCount, 1);
      final entry = session.pendingWrites.single;
      expect(entry.method, 'POST');
      expect(entry.path, '/api/announcements/announcement/7/read');
      expect(entry.body, {'via': 'flutter'});
      expect(entry.key, Outbox.keyFor(
          'POST', '/api/announcements/announcement/7/read', {'via': 'flutter'}));
      expect(session.offline, isTrue);

      // And it is durable, not just in memory: the same bytes are on the
      // device, under the queue's own key.
      final prefs = await SharedPreferences.getInstance();
      final stored = prefs.getStringList(Outbox.queueKey);
      expect(stored, isNotNull);
      expect(stored!.single, contains(entry.key));
    });

    test('the recorded request is the request the online path sends', () async {
      // The queue stores method, path and body to replay later. If that
      // recording drifts from what the client sends when the server is up, the
      // replay is a *different* request than the one the person made — which is
      // the failure this test exists to make impossible.
      final offline = <SentRequest>[];
      final queued = SessionState(client: offlineClient(sent: offline));
      await queued.selfReportDues(tier: 'supported', fiscalYear: 2026);
      final entry = queued.pendingWrites.single;

      final online = <SentRequest>[];
      // A fresh store for this half: the session above left its entry on the
      // device, and an online write drains whatever is owed as well as sending
      // itself — which is the point of the queue, but not what is being measured
      // here.
      SharedPreferences.setMockInitialValues({});
      final live = SessionState(client: liveClient(sent: online));
      await live.selfReportDues(tier: 'supported', fiscalYear: 2026);

      expect(entry.method, online.single.method);
      expect(entry.path, online.single.path);
      expect(entry.body, online.single.body);
      expect(entry.path, '/api/finance/dues/self-report');
      expect(entry.body, {'tier': 'supported', 'fiscal_year': 2026});
    });

    test('the same write queued twice is one entry, not two', () async {
      // A double tap, or a screen rebuilt onto the same receipt.
      final session = SessionState(client: offlineClient());
      await session.markAnnouncementRead('7');
      await session.markAnnouncementRead('7');

      expect(session.pendingWriteCount, 1);
    });

    test('two different writes are two entries, in the order they were made',
        () async {
      final session = SessionState(client: offlineClient());
      await session.markAnnouncementRead('7');
      await session.markAnnouncementUnread('8');

      expect(
        session.pendingWrites.map((e) => e.path),
        [
          '/api/announcements/announcement/7/read',
          '/api/announcements/announcement/8/unread',
        ],
      );
    });

    test('a write the server cannot replay is refused, not queued', () async {
      // An order is appended by the server, so a replay would create a second
      // one. Offline, that is refused in words — and nothing is recorded.
      final session = SessionState(client: offlineClient());

      await expectLater(
        session.placeStoreOrder(
          lines: [
            {'item_id': 1, 'quantity': 2},
          ],
          tier: 'standard',
        ),
        throwsA(isA<OfflineWriteRefused>().having(
          (e) => e.message,
          'message',
          contains('cannot be queued'),
        )),
      );
      expect(session.pendingWriteCount, 0);
    });
  });

  // -------------------------------------------------------------------------
  // Replay
  // -------------------------------------------------------------------------

  group('replay', () {
    test('sends the queue in order and empties it', () async {
      final offline = SessionState(client: offlineClient());
      await offline.markAnnouncementRead('7');
      await offline.markAnnouncementUnread('8');
      await offline.selfReportDues(tier: 'hardship');
      expect(offline.pendingWriteCount, 3);

      // The server comes back.
      final sent = <SentRequest>[];
      final session = SessionState(client: liveClient(sent: sent));
      // This session sees what the last one owed: the queue is on the device,
      // which is exactly what an app relaunch is.
      await session.refreshOutbox();
      expect(session.pendingWriteCount, 3);
      final report = await session.syncOutbox();

      expect(report.sent, 3);
      expect(report.remaining, 0);
      expect(session.pendingWriteCount, 0);
      expect(sent.map((r) => r.path), [
        '/api/announcements/announcement/7/read',
        '/api/announcements/announcement/8/unread',
        '/api/finance/dues/self-report',
      ]);
      expect(sent.first.recorded, {
        'method': 'POST',
        'path': '/api/announcements/announcement/7/read',
        'body': {'via': 'flutter'},
      });
      expect(sent.last.body, {'tier': 'hardship'});
      expect(await Outbox().pending(), isEmpty);
    });

    test('three queued writes replay oldest first, and nothing is lost',
        () async {
      final outbox = Outbox();
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/a', null),
        method: 'POST',
        path: '/api/a',
        queuedAt: DateTime(2026, 9, 27, 8),
      ));
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/b', null),
        method: 'POST',
        path: '/api/b',
        queuedAt: DateTime(2026, 9, 27, 8, 1),
      ));
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/c', null),
        method: 'POST',
        path: '/api/c',
        queuedAt: DateTime(2026, 9, 27, 8, 2),
      ));

      final sent = <SentRequest>[];
      final client = liveClient(sent: sent);
      final report = await outbox.flush(
        send: (entry) => client.send(entry.method, entry.path, body: entry.body),
      );

      expect(report.sent, 3);
      expect(report.remaining, 0);
      expect(sent.map((r) => r.path), ['/api/a', '/api/b', '/api/c']);
      expect(await outbox.pending(), isEmpty);
    });

    test('a transport failure stops the pass and keeps the order behind it',
        () async {
      final outbox = Outbox();
      for (final path in ['/api/a', '/api/b', '/api/c']) {
        await outbox.enqueue(OutboxEntry(
          key: Outbox.keyFor('POST', path, null),
          method: 'POST',
          path: path,
          queuedAt: DateTime(2026, 9, 27, 9),
        ));
      }

      // The server answers for the first write and then dies.
      final seen = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          seen.add(request.url.path);
          if (request.url.path == '/api/b') {
            throw http.ClientException('no route to host');
          }
          return jsonResponse({'ok': true});
        }),
      );

      final first = await outbox.flush(
        send: (entry) => client.send(entry.method, entry.path, body: entry.body),
      );

      expect(seen, ['/api/a', '/api/b']);
      expect(first.sent, 1);
      expect(first.unreachable, isTrue);
      expect(first.remaining, 2);
      // B was never sent ahead of... and C was not sent at all: an entry behind
      // an unsent one does not jump the queue.
      expect((await outbox.pending()).map((e) => e.path), ['/api/b', '/api/c']);

      // When it comes back, the pass resumes where it stopped, still in order.
      final sent = <SentRequest>[];
      final second = await outbox.flush(
        send: (entry) => liveClient(sent: sent).send(entry.method, entry.path),
      );
      expect(second.sent, 2);
      expect(sent.map((r) => r.path), ['/api/b', '/api/c']);
      expect(await outbox.pending(), isEmpty);
    });

    test('two reports made offline replay in the order they were made', () async {
      final outbox = Outbox();
      for (final tier in ['standard', 'hardship']) {
        final body = {'tier': tier};
        await outbox.enqueue(OutboxEntry(
          key: Outbox.keyFor('POST', '/api/finance/dues/self-report', body),
          method: 'POST',
          path: '/api/finance/dues/self-report',
          body: body,
          queuedAt: DateTime(2026, 9, 27, 10),
        ));
      }

      final sent = <SentRequest>[];
      await outbox.flush(
        send: (entry) =>
            liveClient(sent: sent).send(entry.method, entry.path, body: entry.body),
      );

      // Last wins, because it was last — the ordering is the whole reason the
      // queue is a list and not a set.
      expect(
        sent.map((r) => (r.body as Map)['tier']).toList(),
        ['standard', 'hardship'],
      );
    });

    test('a read that lands drains the queue without being asked', () async {
      // "Syncs when online" has to be caused by something. It is caused by the
      // server answering: any response at all proves the server is back.
      final sent = <SentRequest>[];
      var up = false;
      final session = SessionState(
        client: ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            sent.add(SentRequest(
              request.method,
              request.url.path,
              request.body.isEmpty ? null : jsonDecode(request.body),
            ));
            if (!up) throw http.ClientException('no route to host');
            return jsonResponse(request.method == 'GET' ? [] : {'ok': true});
          }),
        ),
      );
      await session.markAnnouncementRead('7');
      expect(session.pendingWriteCount, 1);

      // The server comes back; the next read is what discovers it.
      up = true;
      sent.clear();
      final cached = await session.cachedList('members', session.api.members);

      expect(cached.isStale, isFalse);
      expect(session.pendingWriteCount, 0);
      // The read went first — it is what proved the server is there — then the
      // write that was owed, as the request the queue recorded.
      expect(sent.map((r) => '${r.method} ${r.path}'), [
        'GET /api/membership/members',
        'POST /api/announcements/announcement/7/read',
      ]);
      expect(sent.last.body, {'via': 'flutter'});
    });
  });

  // -------------------------------------------------------------------------
  // The refusal path
  // -------------------------------------------------------------------------

  group('a write the server refuses', () {
    test('is parked with the server\'s words, and does not hold the queue',
        () async {
      final outbox = Outbox();
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/announcements/announcement/9/read', null),
        method: 'POST',
        path: '/api/announcements/announcement/9/read',
        queuedAt: DateTime(2026, 9, 27, 11),
      ));
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/announcements/announcement/10/read', null),
        method: 'POST',
        path: '/api/announcements/announcement/10/read',
        queuedAt: DateTime(2026, 9, 27, 11, 1),
      ));

      final seen = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          seen.add(request.url.path);
          if (request.url.path.endsWith('/9/read')) {
            return jsonResponse(
              {'error': 'not sent to a scope you hold'},
              403,
            );
          }
          return jsonResponse({'is_read': true});
        }),
      );

      final report = await outbox.flush(
        send: (entry) => client.send(entry.method, entry.path, body: entry.body),
      );

      expect(report.refused, 1);
      expect(report.sent, 1);
      expect(report.remaining, 0);
      // It did not block the write behind it...
      expect(seen, [
        '/api/announcements/announcement/9/read',
        '/api/announcements/announcement/10/read',
      ]);
      // ...and it did not vanish: it is kept, with the server's own sentence.
      final refused = await outbox.refused();
      expect(refused, hasLength(1));
      expect(refused.single.statusCode, 403);
      expect(refused.single.message, 'not sent to a scope you hold');
      expect(refused.single.entry.path,
          '/api/announcements/announcement/9/read');
      expect(refused.single.summary, contains('403'));
      expect(refused.single.summary, contains('not sent to a scope you hold'));
      expect(await outbox.pending(), isEmpty);
    });

    test('a 5xx is retried, then parked — it cannot block the queue forever',
        () async {
      final outbox = Outbox();
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/plugins/calendar/enable', null),
        method: 'POST',
        path: '/api/plugins/calendar/enable',
        queuedAt: DateTime(2026, 9, 27, 12),
      ));

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({'error': 'the plugin failed to load'}, 500),
        ),
      );

      // Three passes: the first two keep it, counted.
      for (var pass = 1; pass <= Outbox.maxAttempts - 1; pass++) {
        final report = await outbox.flush(
          send: (entry) =>
              client.send(entry.method, entry.path, body: entry.body),
        );
        expect(report.refused, 0, reason: 'pass $pass');
        expect(report.remaining, 1, reason: 'pass $pass');
        expect((await outbox.pending()).single.attempts, pass);
      }
      final last = await outbox.flush(
        send: (entry) => client.send(entry.method, entry.path, body: entry.body),
      );

      expect(last.refused, 1);
      expect(last.remaining, 0);
      final refused = await outbox.refused();
      expect(refused.single.statusCode, 500);
      expect(refused.single.message, contains('3 times'));
      expect(refused.single.attempts, Outbox.maxAttempts);
    });

    test('the person can dismiss a refusal, and only after reading it', () async {
      var up = false;
      final session = SessionState(
        client: ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((_) async {
            // Offline while the receipt is made, then a refusal when it is
            // replayed: the one path that leaves both lists involved.
            if (!up) throw http.ClientException('no route to host');
            return jsonResponse({'error': 'not sent to a scope you hold'}, 403);
          }),
        ),
      );
      await session.markAnnouncementRead('7');
      expect(session.pendingWriteCount, 1);

      up = true;
      await session.syncOutbox();

      expect(session.pendingWriteCount, 0);
      expect(session.refusedWriteCount, 1);
      expect(session.refusedWrites.single.message,
          'not sent to a scope you hold');

      await session.clearRefusedWrites();
      expect(session.refusedWriteCount, 0);
      expect(await Outbox().refused(), isEmpty);
    });

    test('a refusal survives the process: it is on the device, not in memory',
        () async {
      final outbox = Outbox();
      await outbox.enqueue(OutboxEntry(
        key: Outbox.keyFor('POST', '/api/a', null),
        method: 'POST',
        path: '/api/a',
        queuedAt: DateTime(2026, 9, 27, 13),
      ));
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({'error': 'forbidden'}, 403),
        ),
      );
      await outbox.flush(
        send: (entry) => client.send(entry.method, entry.path, body: entry.body),
      );

      // A brand-new Outbox (a relaunch) reads the same refusal back.
      final afterRelaunch = await Outbox().refused();
      expect(afterRelaunch.single.message, 'forbidden');
    });
  });

  // -------------------------------------------------------------------------
  // The read cache
  // -------------------------------------------------------------------------

  group('the durable read cache', () {
    test('survives an offline window and says when its data is from', () async {
      // Online first: the roster is read and stored.
      final live = SessionState(client: liveClient(sent: [], body: [
        {'id': 'u1', 'display_name': 'Scout One'},
      ]));
      final fresh = await live.cachedList('members', live.api.members);
      expect(fresh.isStale, isFalse);
      expect(fresh.value, hasLength(1));

      final prefs = await SharedPreferences.getInstance();
      expect(prefs.getString('cache.members'), isNotNull);
      expect(prefs.getString('cache.members.at'), isNotNull);

      // Then the woods: a *new* session — a relaunch — with no route to the
      // server. The last-known answer is served, and it is marked stale with
      // the hour it was stored.
      final offline = SessionState(client: offlineClient());
      final cached = await offline.cachedList('members', offline.api.members);

      expect(cached.isStale, isTrue);
      expect(cached.cachedAt, isNotNull);
      // The value is the server's, not an empty list that would read as "no
      // members".
      expect((cached.value.single)['display_name'], 'Scout One');
      expect(offline.offline, isTrue);
    });

    test('a live answer is never served from the cache', () async {
      // The cache must not swallow a response: when the server answers, that
      // answer is what the screen gets, and the cache is written behind it.
      SharedPreferences.setMockInitialValues({
        'cache.members': jsonEncode([
          {'id': 'old', 'display_name': 'Yesterday'},
        ]),
        'cache.members.at': DateTime(2026, 9, 26, 6).toIso8601String(),
      });

      final sent = <SentRequest>[];
      final session = SessionState(
        client: liveClient(sent: sent, body: [
          {'id': 'u1', 'display_name': 'Today'},
        ]),
      );
      final cached = await session.cachedList('members', session.api.members);

      expect(cached.isStale, isFalse);
      expect(cached.value.single['display_name'], 'Today');
      // And it is then what is on the device, replacing the older read.
      final prefs = await SharedPreferences.getInstance();
      expect(prefs.getString('cache.members'), contains('Today'));
    });

    test('signing out drops the cache and the queue with the session', () async {
      // A cached roster that outlives the session is one member's troop shown
      // to the next; a queue that outlives it replays one person's change as
      // whoever signs in next. Both go.
      SharedPreferences.setMockInitialValues({
        'cache.members': jsonEncode([
          {'id': 'u1', 'display_name': 'Scout One'},
        ]),
        'cache.members.at': DateTime(2026, 9, 26, 6).toIso8601String(),
      });
      final session = SessionState(client: offlineClient());
      await session.markAnnouncementRead('7');
      expect(session.pendingWriteCount, 1);

      await session.signOut();

      final prefs = await SharedPreferences.getInstance();
      expect(prefs.getString('cache.members'), isNull);
      expect(prefs.getString('cache.members.at'), isNull);
      expect(prefs.getStringList(Outbox.queueKey), isNull);
      expect(session.pendingWriteCount, 0);
    });
  });

  // -------------------------------------------------------------------------
  // The screen
  // -------------------------------------------------------------------------

  group('AnnouncementsScreen while offline', () {
    testWidgets('a receipt made offline is queued, and the screen says so',
        (tester) async {
      // A cached inbox, and no route to the server: a scout in the woods.
      SharedPreferences.setMockInitialValues({
        'cache.announcements': jsonEncode([
          {
            'id': 7,
            'title': 'Meeting moved to Thursday',
            'category': 'urgent',
            'is_read': false,
            'status': 'published',
          },
        ]),
        'cache.announcements.at': DateTime(2026, 9, 27, 7, 30).toIso8601String(),
      });

      final session = SessionState(client: offlineClient());
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>.value(
          value: session,
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const Scaffold(body: AnnouncementsScreen()),
          ),
        ),
      );
      await tester.pumpAndSettle();

      // The cached inbox is on screen, and the screen admits it is stale.
      expect(find.text('Meeting moved to Thursday'), findsOneWidget);
      expect(find.textContaining('Offline'), findsWidgets);

      await tester.tap(find.text('Mark read'));
      await tester.pumpAndSettle();

      // The tap recorded the receipt on the device rather than pretending it
      // reached the server.
      expect(session.pendingWriteCount, 1);
      expect(session.pendingWrites.single.path,
          '/api/announcements/announcement/7/read');
      expect(find.textContaining('queued and will sync'), findsOneWidget);
    });

    testWidgets('a queued receipt is not applied to the row it belongs to',
        (tester) async {
      // The row must not be flipped locally: the record is the server's, and
      // showing it as read would show a change nobody else can see.
      SharedPreferences.setMockInitialValues({
        'cache.announcements': jsonEncode([
          {
            'id': 7,
            'title': 'Meeting moved to Thursday',
            'category': 'urgent',
            'is_read': false,
            'status': 'published',
          },
        ]),
        'cache.announcements.at': DateTime(2026, 9, 27, 7, 30).toIso8601String(),
      });
      final session = SessionState(client: offlineClient());
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>.value(
          value: session,
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const Scaffold(body: AnnouncementsScreen()),
          ),
        ),
      );
      await tester.pumpAndSettle();
      await tester.tap(find.text('Mark read'));
      await tester.pumpAndSettle();

      // It is still offered as unread, because it still is.
      expect(find.text('Mark read'), findsOneWidget);
    });
  });
}
