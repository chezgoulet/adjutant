/// The client, against a **real, running server** — the half of the v1.0 gate a
/// mock `http.Client` cannot reach.
///
/// The suite in `test/` drives every screen against fixtures the test itself
/// holds. That is worth having, and it proves the client renders the shapes it
/// expects; what it cannot prove is that those shapes are the ones the server
/// sends. A client that has drifted from the API answers every mock perfectly.
///
/// So this harness uses the product's own [ApiClient] over a real socket and
/// asserts on **what the server returned**: a session the server issued, the
/// identity the server resolved, and roster rows the server stored. Nothing here
/// is a fixture standing in for the server's answer.
///
/// **It fails loudly when the server is absent.** A probe nobody runs, or one
/// that silently skips, is not evidence: if `ADJUTANT_LIVE_BASE` is not
/// listening, this file reports one failing test naming the address and never
/// reaches a passing state. There is no `skip`, no tolerance for an unreachable
/// host, and no mock fallback.
///
/// Run it through `scripts/client-live-harness.sh`, which creates the harness
/// database, boots a server on it and points this file at it:
///
///     scripts/client-live-harness.sh
///
/// or, against a server you started yourself:
///
///     cd client && ADJUTANT_LIVE_BASE=http://127.0.0.1:8790 \
///       flutter test live/live_client_test.dart
///
/// This file deliberately lives outside `test/`, so `flutter test` (the client
/// job's existing gate, 104 mock-client tests) does not pick it up and does not
/// need a server to stay green.
library;

import 'dart:convert';
import 'dart:io';

import 'package:adjutant_client/api/api_client.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;

/// The live server's address. The boot script passes it through; the default is
/// the port `scripts/client-live-harness.sh` binds.
String get _baseUrl =>
    Platform.environment['ADJUTANT_LIVE_BASE'] ?? 'http://127.0.0.1:8790';

/// How many server-verified probes this file contains. The last test compares it
/// with the number that actually ran, so a probe deleted rather than fixed —
/// or one whose body was emptied into a no-op — breaks the gate instead of
/// quietly shrinking it.
const int _expectedProbes = 9;

/// The identity this harness bootstraps for itself. It owns nothing: the server
/// is booted against a database created for this run, so the first registration
/// is the bootstrap chief.
const String _username = 'client-harness-chief';
const String _password = 'client-harness-bootstrap-pass';

/// One line of the probe log, so a CI run can be read the way the Python
/// harnesses' runs are read (`PASS 200 name`).
void _record(bool ok, String name, String detail) {
  _state.ran++;
  // ignore: avoid_print
  print('${ok ? 'PASS' : 'FAIL'}  $name  ($detail)');
  if (!ok) _state.failed++;
}

/// The running tally the final probe checks.
final _State _state = _State();

class _State {
  int ran = 0;
  int failed = 0;
}

void _expect(bool ok, String name, String detail) {
  _record(ok, name, detail);
  if (!ok) {
    fail('$name — $detail');
  }
}

String _short(String text, [int limit = 160]) {
  final flat = text.replaceAll('\n', ' ');
  return flat.length <= limit ? flat : '${flat.substring(0, limit)}…';
}

/// The message a missing server gets. It is the whole point of file: an
/// unreachable host is a failure, stated once, at the top, with the fix.
String _unreachable(Object cause) => '''

=============================================================================
LIVE SERVER UNREACHABLE at $_baseUrl — this harness refuses to pass.
-----------------------------------------------------------------------------
This is the client half of the v1.0 gate: it drives the real Flutter client
against a real running server and asserts on what the SERVER returned. There
is no mock and no skip here — an absent server is a FAILED gate.

Boot one with the script that owns this contract:

    scripts/client-live-harness.sh

Underlying error: $cause
=============================================================================
''';

/// A minimal raw HTTP client, used only to build the fixture the product client
/// then reads back. Everything asserted below comes through [ApiClient].
class _Raw {
  final http.Client _client = http.Client();

  Future<http.Response> send(
    String method,
    String path, {
    Object? body,
    String? token,
    Map<String, String>? headers,
  }) async {
    final request = http.Request(method, Uri.parse('$_baseUrl$path'));
    request.headers['accept'] = 'application/json';
    if (body != null) {
      request.headers['content-type'] = 'application/json; charset=utf-8';
      request.body = jsonEncode(body);
    }
    if (token != null) request.headers['authorization'] = 'Bearer $token';
    headers?.forEach((key, value) => request.headers[key] = value);
    final streamed = await _client.send(request);
    return http.Response.fromStream(streamed);
  }

  void close() => _client.close();
}

Map<String, dynamic> _decode(http.Response response) {
  if (response.body.isEmpty) return const {};
  try {
    final decoded = jsonDecode(utf8.decode(response.bodyBytes));
    if (decoded is Map) return Map<String, dynamic>.from(decoded);
  } on FormatException {
    // Not JSON: the caller reports the status and the raw text instead.
  }
  return const {};
}

void main() {
  final uri = Uri.parse(_baseUrl);

  // The precondition, checked before a single probe is registered. It is
  // deliberately synchronous and deliberately outside `setUpAll`: a server that
  // is not there must produce one unambiguous failure, and must not leave a
  // group of probes that could report anything else.
  try {
    RawSynchronousSocket.connectSync(uri.host, uri.port).closeSync();
  } catch (error) {
    test('0 the live server at $_baseUrl is reachable', () {
      fail(_unreachable(error));
    });
    return;
  }

  group('the client against a live server at $_baseUrl', () {
    final raw = _Raw();
    late String chiefToken;
    late Map<String, dynamic> fixtureMember;
    late Map<String, dynamic> fixtureMotion;

    // Fixtures are built as the bootstrap chief over raw HTTP, because the
    // product client deliberately has no member-create or motion-create call —
    // it is a renderer for server state. What the client is pointed at below is
    // therefore real server state, written by the server and read back through
    // the product's own code.
    setUpAll(() async {
      final health = await raw.send('GET', '/');
      if (health.statusCode != 200) {
        throw StateError(
            'the server at $_baseUrl answered ${health.statusCode} on GET /: '
            '${_short(health.body)}');
      }

      final registered = await raw.send('POST', '/api/auth/register',
          body: {'username': _username, 'password': _password});
      if (registered.statusCode == 201) {
        chiefToken = _decode(registered)['token'].toString();
      } else if (registered.statusCode == 403) {
        // Not pristine: somebody already bootstrapped this database. Reuse the
        // identity rather than inventing one the server does not have.
        final login = await raw.send('POST', '/api/auth/login',
            body: {'username': _username, 'password': _password});
        if (login.statusCode != 200) {
          throw StateError(
              'database is already bootstrapped and $_username cannot log in '
              '(${login.statusCode}: ${_short(login.body)}); rerun through '
              'scripts/client-live-harness.sh, which creates a fresh database');
        }
        chiefToken = _decode(login)['token'].toString();
      } else {
        throw StateError('bootstrap register answered ${registered.statusCode}: '
            '${_short(registered.body)}');
      }

      Future<void> expect201(String name, Future<http.Response> call) async {
        final response = await call;
        if (response.statusCode != 201) {
          throw StateError(
              '$name answered ${response.statusCode}: ${_short(response.body)}');
        }
      }

      await expect201(
          'create lodge',
          raw.send('POST', '/api/membership/lodge',
              body: {'name': 'Harness Lodge'}, token: chiefToken));
      await expect201(
          'create patrol',
          raw.send('POST', '/api/membership/patrol',
              body: {'name': 'Harness Patrol', 'lodge': 'Harness Lodge'},
              token: chiefToken));

      final member = await raw.send('POST', '/api/membership/member',
          token: chiefToken,
          body: {
            'username': 'harness-scout',
            'display_name': 'Harness Scout',
            'trail_name': 'Beacon',
            'patrol': 'Harness Patrol',
          });
      if (member.statusCode != 201) {
        throw StateError('create member answered ${member.statusCode}: '
            '${_short(member.body)}');
      }
      fixtureMember = _decode(member);

      final meeting = await raw.send('POST', '/api/governance/meeting',
          token: chiefToken,
          body: {
            'body': 'tc',
            'title': 'Harness Council — live client gate',
            'scheduled_for': '2026-12-06T18:00:00Z',
            'quorum_basis': 'majority_members',
            'expected_voters': 5,
            'location': 'Harness Lodge',
          });
      if (meeting.statusCode != 201) {
        throw StateError('create meeting answered ${meeting.statusCode}: '
            '${_short(meeting.body)}');
      }
      final meetingId = _decode(meeting)['meeting'] is Map
          ? _decode(meeting)['meeting']['id']
          : _decode(meeting)['id'];
      if (meetingId == null) {
        throw StateError('meeting id missing: ${_short(meeting.body)}');
      }
      await raw.send('POST', '/api/governance/meeting/$meetingId/open',
          token: chiefToken);

      final motion = await raw.send('POST', '/api/governance/motion',
          token: chiefToken,
          body: {
            'title': 'Harness motion — live client gate',
            'text': 'that the harness prove itself against a real server',
            'body': 'tc',
            'meeting_id': meetingId,
            'category': 'general',
            'threshold': 'simple_majority',
          });
      if (motion.statusCode != 201) {
        throw StateError('create motion answered ${motion.statusCode}: '
            '${_short(motion.body)}');
      }
      fixtureMotion = _decode(motion)['motion'] is Map
          ? Map<String, dynamic>.from(_decode(motion)['motion'])
          : _decode(motion);
    });

    tearDownAll(() => raw.close());

    test('1 the server answers its own health route', () async {
      final response = await raw.send('GET', '/');
      _expect(
        response.statusCode == 200 && response.body.contains('"status":"ok"'),
        'server health',
        'GET / -> ${response.statusCode} ${_short(response.body)}',
      );
    });

    test('2 the dev-header identity stub is OFF', () async {
      // The precondition that makes the probes below evidence. With the stub on,
      // these two headers would BE the identity and everything after this would
      // prove nothing about sessions. A refusal is required, and the client
      // holds no such headers, so the identity it gets must come from the
      // server's own session provider.
      final response = await raw.send('GET', '/api/membership/members',
          headers: {
            'x-dev-user': 'client-harness-stub-must-not-work',
            'x-dev-role': 'chief',
          });
      _expect(
        response.statusCode == 401 || response.statusCode == 403,
        'dev-header stub is OFF',
        'spoofed x-dev-user refused with ${response.statusCode}',
      );
    });

    test('3 ApiClient.login gets a session the server issued', () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      final token = await api.login(username: _username, password: _password);
      _expect(
        token.isNotEmpty,
        'login issues a session',
        'POST /api/auth/login -> token of ${token.length} chars',
      );
    });

    test('4 ApiClient.me resolves the identity the server holds', () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      await api.login(username: _username, password: _password);
      final me = await api.me();
      final roles = (me['roles'] as List?)?.map((e) => '$e').toList() ?? const [];
      _expect(
        me['username'] == _username && roles.contains('chief'),
        'me reflects the server identity',
        'username=${me['username']} roles=$roles',
      );
    });

    test('5 ApiClient.members returns the row the server stored', () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      await api.login(username: _username, password: _password);
      final members = await api.members();
      final found = members.where((m) => m['username'] == 'harness-scout');
      _expect(
        found.length == 1 &&
            found.first['display_name'] == 'Harness Scout' &&
            found.first['trail_name'] == 'Beacon' &&
            found.first['id'] == fixtureMember['id'],
        'members carries the stored roster row',
        'id=${fixtureMember['id']} rows=${members.length} '
            'found=${found.isEmpty ? 'none' : jsonEncode(_pick(found.first))}',
      );
    });

    test('6 ApiClient.lodges returns the lodge the server owns', () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      await api.login(username: _username, password: _password);
      final lodges = await api.lodges();
      final names = lodges.map((l) => '${l['name']}').toList();
      _expect(
        names.contains('Harness Lodge'),
        'lodges carries the created lodge',
        'names=$names',
      );
    });

    test('7 ApiClient.motions returns the motion with its server stage',
        () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      await api.login(username: _username, password: _password);
      final motions = await api.motions();
      final found =
          motions.where((m) => m['title'] == 'Harness motion — live client gate');
      // `stage` and `result` are the server's own vocabulary; asserting on them
      // here is what catches a client that has started guessing stage names.
      _expect(
        found.length == 1 && found.first['stage'] == 'proposed',
        'motions carries the server stage',
        'id=${fixtureMotion['id']} rows=${motions.length} '
            'stage=${found.isEmpty ? 'none' : found.first['stage']}',
      );
    });

    test('8 ApiClient.plugins lists what the server actually loaded', () async {
      final api = ApiClient(baseUrl: _baseUrl);
      addTearDown(api.close);
      await api.login(username: _username, password: _password);
      final page = await api.plugins();
      final ids = jsonEncode(page);
      _expect(
        ids.contains('auth'),
        'plugins lists the loaded registry',
        'keys=${page.keys.toList()} body=${_short(ids, 200)}',
      );
    });

    test('9 ApiClient surfaces the server refusal for a dead session',
        () async {
      final api = ApiClient(baseUrl: _baseUrl)..setToken('not-a-session');
      addTearDown(api.close);
      // The refusal has to be the *server's* answer, not a client-side guess:
      // an unexpected status here means this client is not talking to the API
      // this harness thinks it is.
      try {
        await api.me();
        fail('a dead session was accepted by the server');
      } on ApiException catch (e) {
        _expect(
          e.statusCode == 401 || e.statusCode == 403,
          'dead session refused',
          'GET /api/auth/me with a bogus token -> ${e.statusCode} ${_short(e.message, 80)}',
        );
      }
    });

    test('10 every probe above actually ran', () {
      // The tally: `_expectedProbes` server-verified probes, none skipped, none
      // silently dropped. A probe that disappears must fail the gate.
      _expect(
        _state.ran == _expectedProbes && _state.failed == 0,
        'probe tally',
        'ran=${_state.ran} expected=$_expectedProbes failed=${_state.failed}',
      );
    });
  });
}

Map<String, dynamic> _pick(Map<String, dynamic> row) => {
      'id': row['id'],
      'username': row['username'],
      'display_name': row['display_name'],
      'trail_name': row['trail_name'],
    };
