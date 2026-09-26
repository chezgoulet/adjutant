import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// The Server field accepted an address, saved it, and then every request went to
/// the compiled-in default — the client had no way to change its base URL at all,
/// so the field had never had any effect, in either direction:
///
///   * `signIn` saved the typed address but called `login()` on a client still
///     pointed at the default, so even the *first* sign-in went to `localhost`;
///   * `boot` read the saved address into a local and dropped it, so every later
///     launch came up on the default, failed, and showed the cached user as
///     offline.
///
/// These two tests are the behaviour that was missing. They assert on the URL the
/// request actually went to, not just on the field, because the bug was precisely
/// that the field and the request disagreed.
http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('signIn talks to the address the user typed, not the default', () async {
    SharedPreferences.setMockInitialValues({});
    final seen = <Uri>[];
    final api = ApiClient(
      baseUrl: 'http://localhost:8080',
      httpClient: MockClient((request) async {
        seen.add(request.url);
        return request.url.path == '/api/auth/login'
            ? _json({'token': 'tok'})
            : _json({'username': 'bea', 'display_name': 'Bea', 'roles': []});
      }),
    );

    await SessionState(client: api).signIn(
      baseUrl: 'https://lodge.example.org',
      username: 'bea',
      password: 'hunter2',
    );

    expect(api.baseUrl, 'https://lodge.example.org');
    expect(seen, isNotEmpty);
    for (final url in seen) {
      expect(url.host, 'lodge.example.org',
          reason: 'every request must go to the typed address');
    }
    expect(seen.first.path, '/api/auth/login');
  });

  test('boot adopts the saved address before asking for the session', () async {
    SharedPreferences.setMockInitialValues({
      'adjutant.baseUrl': 'https://lodge.example.org',
      'adjutant.token': 'tok',
    });
    final seen = <Uri>[];
    final api = ApiClient(
      baseUrl: 'http://localhost:8080',
      httpClient: MockClient((request) async {
        seen.add(request.url);
        return _json({'username': 'bea', 'display_name': 'Bea', 'roles': []});
      }),
    );

    final session = SessionState(client: api);
    await session.boot();

    expect(api.baseUrl, 'https://lodge.example.org',
        reason: 'the saved address is the one to use, not the default');
    expect(seen.single.host, 'lodge.example.org',
        reason: 'the session check must go to the saved box');
    expect(session.user, isNotNull,
        reason: 'and it must not be reported as offline when it answered');
    expect(session.offline, isFalse);
  });

  test('boot leaves the address alone when nothing was ever saved', () async {
    SharedPreferences.setMockInitialValues({});
    final api = ApiClient(
      baseUrl: 'http://localhost:8080',
      httpClient: MockClient((_) async => _json({})),
    );

    await SessionState(client: api).boot();

    expect(api.baseUrl, 'http://localhost:8080',
        reason: 'a first run has no saved address, so the default holds');
  });
}
