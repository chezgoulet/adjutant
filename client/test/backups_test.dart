import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/backups_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';

/// The backup surface: where the requests go, what the schedule sends, and what
/// the admin is told when the server refuses.
http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

Widget _wrap(SessionState session, Widget child) =>
    ChangeNotifierProvider<SessionState>.value(
      value: session,
      child: MaterialApp(theme: AppTheme.light(), home: child),
    );

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('the backup routes', () {
    test('the list is a GET of the one endpoint that has all three answers',
        () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return _json({
            'presets': [
              {'seconds': 3600, 'label': 'hourly'}
            ],
            'schedule': {'cadence_secs': 86400, 'keep': 14, 'enabled': true},
            'runs': [],
            'directory': '/var/backups/adjutant',
          });
        }),
      );

      final data = await client.backups();

      expect(calls, ['GET /api/backups']);
      expect(data['directory'], '/var/backups/adjutant');
    });

    test('enabling a schedule sends an explicit cadence', () async {
      late Map<String, dynamic> sent;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          sent = jsonDecode(request.body) as Map<String, dynamic>;
          return _json({'schedule': {}});
        }),
      );

      await client.setBackupSchedule(
        cadenceSecs: 21600,
        keep: 30,
        enabled: true,
      );

      expect(sent['cadence_secs'], 21600);
      expect(sent['keep'], 30);
      expect(sent['enabled'], true);
    });

    test('turning the schedule off sends null, not an omitted key', () async {
      // The route reads an absent key as "leave the cadence alone" and an
      // explicit null as "there is no schedule". Sending the wrong one leaves a
      // cadence set behind a switch that says off, which is the kind of state a
      // troop only discovers from a bill.
      late Map<String, dynamic> sent;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          sent = jsonDecode(request.body) as Map<String, dynamic>;
          return _json({'schedule': {}});
        }),
      );

      await client.setBackupSchedule(cadenceSecs: null, keep: 14, enabled: false);

      expect(sent.containsKey('cadence_secs'), isTrue);
      expect(sent['cadence_secs'], isNull);
      expect(sent['enabled'], false);
    });

    test('running one now posts to the run route', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return _json({'run': {'id': 1, 'status': 'done'}});
        }),
      );

      final data = await client.runBackupNow();

      expect(calls, ['POST /api/backups/run']);
      expect((data['run'] as Map)['status'], 'done');
    });

    test('a bundle is fetched as bytes, and a refusal is an ApiException',
        () async {
      // A `pg_dump` archive is not JSON, so this path must not decode it — and a
      // refusal has to arrive as a status the screen can branch on rather than as
      // an empty file the admin would save and only discover was empty later.
      final ok = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          expect(request.url.path, '/api/backups/adjutant-1.dump/download');
          return http.Response.bytes([1, 2, 3, 4], 200);
        }),
      );
      expect(await ok.downloadBackup('adjutant-1.dump'), [1, 2, 3, 4]);

      final refused = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => _json({'error': 'insufficient permissions'}, 403),
        ),
      );
      await expectLater(
        refused.downloadBackup('adjutant-1.dump'),
        throwsA(
          isA<ApiException>().having((e) => e.statusCode, 'statusCode', 403),
        ),
      );
    });
  });

  group('the backups screen', () {
    testWidgets('a refusal is the server\'s answer, naming the grant',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => _json({'error': 'insufficient permissions'}, 403),
        ),
      );

      await tester.pumpWidget(
        _wrap(SessionState(client: client), const BackupsScreen()),
      );
      await tester.pumpAndSettle();

      // The grant is named, and named as *its own* grant — an admin who is not
      // told that will go looking for the wrong permission.
      expect(find.textContaining('core:backup'), findsOneWidget);
    });

    testWidgets('the schedule and the bundles render', (tester) async {
      // A phone-sized viewport does not build the rows below the fold, and an
      // unbuilt widget is invisible to `find.text` — the assertion would fail on
      // a screen that is perfectly correct. Give the test enough room that every
      // row is built, so the test is about the screen and not about scrolling.
      tester.view.physicalSize = const Size(1200, 4000);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path != '/api/backups') {
            return _json({}, 404);
          }
          return _json({
            'presets': [
              {'seconds': 3600, 'label': 'hourly'},
              {'seconds': 86400, 'label': 'daily'},
            ],
            'schedule': {'cadence_secs': 86400, 'keep': 30, 'enabled': true},
            'runs': [
              {
                'created_at': '2026-09-26T18:30:00Z',
                'trigger': 'manual',
                'status': 'done',
                'filename': 'adjutant-2026-09-26-183000.dump',
                'bytes': 242369,
              },
              {
                'created_at': '2026-09-26T02:00:00Z',
                'trigger': 'scheduled',
                'status': 'done',
                'filename': 'adjutant-2026-09-26-020000.dump',
                'bytes': 240000,
                'unrecorded': true,
              },
            ],
            'directory': '/var/backups/adjutant',
          });
        }),
      );

      await tester.pumpWidget(
        _wrap(SessionState(client: client), const BackupsScreen()),
      );
      await tester.pumpAndSettle();

      expect(find.text('Back up automatically'), findsOneWidget);
      expect(find.text('Back up now'), findsOneWidget);
      expect(find.text('adjutant-2026-09-26-183000.dump'), findsOneWidget);
      expect(find.textContaining('236.7 KB'), findsOneWidget);
      // A bundle nobody recorded says so, rather than looking like any other.
      expect(find.textContaining('not recorded by this server'), findsOneWidget);
    });
  });
}
