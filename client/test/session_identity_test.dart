/// #166, proved by its negatives.
///
/// A 401 at boot used to clear the token and the user and nothing else, so
/// identity B could be served identity A's cached roster offline, and A's
/// queued write replayed under B's bearer token. These tests assert on what is
/// *stored* and what was *sent*, and each one is written so that the old
/// behaviour fails it:
///
/// * after a 401 boot, the cache and the queue are gone;
/// * B's session never reads A's cache key;
/// * B's replay pass never sends A's entry (and does not delete it);
/// * A's own replay pass still sends it — the feature that must not break.
library;

import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/state/outbox.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/state/token_store.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';

http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

String _entryLine({
  required String owner,
  String path = '/api/finance/dues/self-report',
  Map<String, Object>? body,
  String queuedAt = '2026-10-01T09:00:00.000',
}) =>
    jsonEncode({
      'key': Outbox.keyFor('POST', path, body, owner: owner),
      'owner': owner,
      'method': 'POST',
      'path': path,
      'body': ?body,
      'queued_at': queuedAt,
      'attempts': 0,
    });

void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  test('a 401 at boot clears the cache and the outbox, as signOut does',
      () async {
    SharedPreferences.setMockInitialValues({
      'adjutant.baseUrl': 'http://example.test',
      'adjutant.token': 'token-A',
      'adjutant.user': jsonEncode({
        'id': 'uA',
        'username': 'alice',
        'display_name': 'Alice',
        'roles': ['scout'],
      }),
      'cache.uA.members': jsonEncode([
        {'id': 'uA', 'display_name': 'Identity A — secret roster'},
      ]),
      'cache.uA.members.at': DateTime(2026, 10, 1, 9).toIso8601String(),
      Outbox.queueKey: [
        _entryLine(owner: 'uA', body: {'tier': 'hardship'}),
      ],
    });

    final tokens = InMemoryTokenStore('token-A');
    final session = SessionState(
      client: ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((_) async => _json({'error': 'unauthorized'}, 401)),
      ),
      tokenStore: tokens,
    );

    await session.boot();

    expect(session.user, isNull);
    expect(session.ownerId, isNull);
    expect(tokens.token, isNull, reason: 'the secure store is cleared too');

    final prefs = await SharedPreferences.getInstance();
    expect(prefs.getString('adjutant.token'), isNull);
    expect(prefs.getString('adjutant.user'), isNull);
    expect(prefs.getString('cache.uA.members'), isNull,
        reason: 'a 401 is the end of the session, so the cache dies with it');
    expect(prefs.getStringList(Outbox.queueKey), isNull,
        reason: 'and so does the queue — A\'s write is not owed to B');
    expect(session.pendingWriteCount, 0);
  });

  test('the next identity is not served the previous identity\'s cache',
      () async {
    // The cache key is A's. B boots offline; even with the artefact still on
    // disk (a build that predates the 401 repair, a storage fault) it is not a
    // name B's session looks up.
    SharedPreferences.setMockInitialValues({
      'adjutant.baseUrl': 'http://example.test',
      'adjutant.user': jsonEncode({
        'id': 'uB',
        'username': 'bob',
        'display_name': 'Bob',
        'roles': ['scout'],
      }),
      'cache.uA.members': jsonEncode([
        {'id': 'uA', 'display_name': 'Identity A — secret roster'},
      ]),
      'cache.uA.members.at': DateTime(2026, 10, 1, 9).toIso8601String(),
    });

    final session = SessionState(
      client: ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((_) async {
          throw http.ClientException('no route to host');
        }),
      ),
      tokenStore: InMemoryTokenStore('token-B'),
    );

    await session.boot();
    expect(session.ownerId, 'uB');

    final cached = await session.cachedList('members', session.api.members);

    expect(cached.isStale, isTrue);
    expect(cached.value, isEmpty,
        reason: 'B is offline; A\'s roster is not B\'s last-known answer');
  });

  test('a queued write does not replay under a different sign-in', () async {
    SharedPreferences.setMockInitialValues({
      'adjutant.baseUrl': 'http://example.test',
      Outbox.queueKey: [
        _entryLine(owner: 'uA', body: {'tier': 'hardship'}),
      ],
    });

    final sent = <String>[];
    final session = SessionState(
      client: ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          sent.add('${request.method} ${request.url.path}');
          if (request.url.path == '/api/auth/login') {
            return _json({'token': 'token-B'});
          }
          if (request.url.path == '/api/auth/me') {
            return _json({
              'id': 'uB',
              'username': 'bob',
              'display_name': 'Bob',
              'roles': ['scout'],
            });
          }
          return _json({'ok': true});
        }),
      ),
      tokenStore: InMemoryTokenStore(),
    );

    await session.signIn(
      baseUrl: 'http://example.test',
      username: 'bob',
      password: 'hunter2',
    );

    expect(session.ownerId, 'uB');
    expect(sent, isNot(contains('POST /api/finance/dues/self-report')),
        reason: 'A\'s dues tier must never be written to B\'s record');
    expect(session.pendingWriteCount, 0, reason: 'none of it is B\'s to show');

    // A's entry was refused, not destroyed: it is still on disk for A.
    final stillOwed = await Outbox().pending(owner: 'uA');
    expect(stillOwed, hasLength(1));
  });

  test('the same identity\'s queue still replays', () async {
    SharedPreferences.setMockInitialValues({
      Outbox.queueKey: [
        _entryLine(owner: 'uA', body: {'tier': 'hardship'}),
      ],
    });

    final sent = <String>[];
    final session = SessionState(
      client: ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          sent.add('${request.method} ${request.url.path}');
          return _json({'ok': true});
        }),
      ),
      tokenStore: InMemoryTokenStore('token-A'),
    )..user = {'id': 'uA', 'username': 'alice'};

    await session.refreshOutbox();
    expect(session.pendingWriteCount, 1);

    final report = await session.syncOutbox();

    expect(report.sent, 1);
    expect(sent, contains('POST /api/finance/dues/self-report'));
    expect(session.pendingWriteCount, 0);
    expect(await Outbox().pending(owner: 'uA'), isEmpty);
  });
}
