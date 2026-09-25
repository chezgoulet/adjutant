import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/plugins_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:adjutant_client/widgets/common.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';

/// The tests that earn their place: the pure logic the UI depends on, and the
/// two widgets whose whole job is to be legible under bad conditions.
void main() {
  group('field()', () {
    test('returns the first present, non-empty key', () {
      final map = {'title': 'Operation Slipperyskin', 'stage': 'execution'};
      expect(field(map, ['title']), 'Operation Slipperyskin');
      expect(field(map, ['missing', 'stage']), 'execution');
    });

    test('skips nulls and blank strings', () {
      final map = {'a': null, 'b': '   ', 'c': 'found'};
      expect(field(map, ['a', 'b', 'c']), 'found');
    });

    test('joins lists rather than printing them as objects', () {
      expect(field({'tags': ['hunt', 'winter']}, ['tags']), 'hunt, winter');
    });

    test('returns the fallback when nothing matches', () {
      expect(field({'a': null}, ['a', 'b'], fallback: 'Untitled'), 'Untitled');
    });
  });

  group('formatDate()', () {
    test('renders a plain date without time', () {
      expect(formatDate('2026-12-14T00:00:00Z'), matches(r'^\d{2}/\d{2}/2026$'));
    });

    test('renders 24-hour time when asked', () {
      // 18:00 UTC rendered in local time — assert the shape, not the hour, so
      // the test does not depend on the machine's timezone.
      expect(formatDate('2026-12-14T18:00:00Z', withTime: true), matches(r'^\d{2}/\d{2}/2026 \d{2}:\d{2}$'));
    });

    test('passes through unparseable input rather than crashing', () {
      expect(formatDate('not a date'), 'not a date');
      expect(formatDate(''), '—');
      expect(formatDate(null), '—');
    });
  });

  group('formatRelativeDate()', () {
    test('names today and tomorrow', () {
      final now = DateTime.now();
      expect(formatRelativeDate(now.toIso8601String()), 'Today');
      expect(formatRelativeDate(now.add(const Duration(days: 1)).toIso8601String()), 'Tomorrow');
      expect(formatRelativeDate(now.subtract(const Duration(days: 1)).toIso8601String()), 'Yesterday');
    });
  });

  group('StatusBadge', () {
    Widget wrap(Widget child, {Brightness brightness = Brightness.light}) => MaterialApp(
          theme: brightness == Brightness.dark ? AppTheme.dark() : AppTheme.light(),
          home: Scaffold(body: child),
        );

    testWidgets('states the status in words, not colour alone', (tester) async {
      await tester.pumpWidget(wrap(const StatusBadge('execution')));
      expect(find.text('In Progress'), findsOneWidget);
    });

    testWidgets('maps the lifecycle stages to human wording', (tester) async {
      for (final entry in {
        'request': 'Requested',
        'review': 'In Review',
        'approval': 'Awaiting Approval',
        'approved': 'Approved',
        'debrief': 'Debrief',
        'report': 'Report',
        'passed': 'Passed',
      }.entries) {
        await tester.pumpWidget(wrap(StatusBadge(entry.key)));
        expect(find.text(entry.value), findsOneWidget, reason: 'stage ${entry.key}');
      }
    });

    testWidgets('renders in dark mode', (tester) async {
      await tester.pumpWidget(
        wrap(const StatusBadge('approved'), brightness: Brightness.dark),
      );
      expect(find.text('Approved'), findsOneWidget);
    });
  });

  group('OfflineBanner', () {
    testWidgets('says what is happening and when the data is from', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: OfflineBanner(cachedAt: DateTime(2026, 12, 14, 18, 5)),
          ),
        ),
      );
      expect(find.textContaining('Offline'), findsOneWidget);
      expect(find.textContaining('18:05'), findsOneWidget);
    });
  });

  group('EmptyState', () {
    testWidgets('always offers a way forward, never a blank screen', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: EmptyState(
              icon: Icons.flag_outlined,
              title: 'No missions yet',
              message: 'Propose one to get started.',
              action: FilledButton(onPressed: () {}, child: const Text('Create Mission')),
            ),
          ),
        ),
      );
      expect(find.text('No missions yet'), findsOneWidget);
      expect(find.text('Create Mission'), findsOneWidget);
    });
  });

  group('ApiClient error mapping', () {
    test('carries the status code so callers can branch on it', () {
      final e = ApiException(401, 'invalid credentials');
      expect(e.isUnauthorized, isTrue);
      expect(e.isForbidden, isFalse);
      expect(e.message, 'invalid credentials');
    });

    test('exposes an offline state distinct from an error', () {
      expect(OfflineException().toString(), contains('OfflineException'));
    });
  });

  group('AppTheme', () {
    test('light and dark are both built and differ in surface', () {
      expect(AppTheme.light().brightness, Brightness.light);
      expect(AppTheme.dark().brightness, Brightness.dark);
      expect(
        AppTheme.light().colorScheme.surface,
        isNot(AppTheme.dark().colorScheme.surface),
      );
    });

    test('touch targets are never below the outdoor minimum', () {
      expect(AppSpacing.touchTargetMin, greaterThanOrEqualTo(48.0));
    });
  });

  group('Plugin administration', () {
    // The verb is the whole payload here: posting the wrong one disables a
    // plugin the operator asked to enable, which is the kind of mistake that
    // only shows up as a troop's calendar going quiet.
    test('enable and disable post the verb that was asked for', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return http.Response('{}', 200);
        }),
      );

      await client.setPluginEnabled('conflicts', true);
      await client.setPluginEnabled('conflicts', false);

      expect(calls, [
        'POST /api/plugins/conflicts/enable',
        'POST /api/plugins/conflicts/disable',
      ]);
    });

    test('a refusal arrives as a status the screen can branch on', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );

      await expectLater(
        client.plugins(),
        throwsA(
          isA<ApiException>()
              .having((e) => e.statusCode, 'statusCode', 403)
              .having((e) => e.message, 'message', 'forbidden'),
        ),
      );
    });

    test('the admin payload survives being read as a map', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"plugins":[{"id":"conflicts","name":"Conflicts","version":"0.2.0",'
            '"enabled":true,"routes":11,"kind":"native","isolated":true,'
            '"permissions":["conflicts:file"],"schedules":[],"route_list":[]}],'
            '"retired_libraries":0,"bound_subscriptions":["conflicts"]}',
            200,
          ),
        ),
      );

      final data = await client.plugins();
      expect(data['plugins'], isA<List>());
      expect((data['plugins'] as List).first['id'], 'conflicts');
      expect(data['bound_subscriptions'], ['conflicts']);
    });
  });

  group('PluginsScreen', () {
    /// Two plugins, as the core reports them.
    String payload() => jsonEncode({
          'plugins': [
            {
              'id': 'conflicts',
              'name': 'Conflicts',
              'version': '0.2.0',
              'enabled': true,
              'routes': 11,
              'kind': 'native',
              'isolated': true,
              'permissions': ['conflicts:file', 'conflicts:read_own'],
              'schedules': [],
              'route_list': [],
            },
            {
              'id': 'calendar',
              'name': 'Calendar',
              'version': '0.2.0',
              'enabled': false,
              'routes': 14,
              'kind': 'native',
              'isolated': true,
              'permissions': ['calendar:read'],
              'schedules': [],
              'route_list': [],
            },
          ],
          'retired_libraries': 0,
          'bound_subscriptions': ['conflicts'],
        });

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>.value(
          value: SessionState(client: client),
          child: const MaterialApp(home: PluginsScreen()),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('lists what is loaded, and says which are off', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((_) async => http.Response(payload(), 200)),
      );
      await pump(tester, client);

      expect(find.text('Conflicts'), findsOneWidget);
      expect(find.text('Calendar'), findsOneWidget);
      // The off one is stated, not merely greyed: an operator scans this list.
      expect(find.text('On'), findsOneWidget);
      expect(find.text('Off'), findsOneWidget);
      expect(find.textContaining('2 loaded'), findsOneWidget);
      // Facts an operator needs to make the call.
      expect(find.text('11 routes'), findsOneWidget);
      expect(find.text('isolated'), findsNWidgets(2));
      expect(find.text('subscribed'), findsOneWidget);
    });

    testWidgets('disabling asks first, then posts the disable verb', (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'GET') return http.Response(payload(), 200);
          return http.Response('{}', 200);
        }),
      );
      await pump(tester, client);

      // The first switch belongs to the first plugin — Conflicts.
      await tester.tap(find.byType(Switch).first);
      await tester.pumpAndSettle();

      // Nothing has been sent yet: taking a plugin offline is not a slip of the
      // thumb, and the dialog says what it costs.
      expect(find.text('Disable Conflicts?'), findsOneWidget);
      expect(calls.where((c) => c.startsWith('POST')), isEmpty);

      await tester.tap(find.text('Disable'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/plugins/conflicts/disable'));
    });

    testWidgets('keeping it on sends nothing at all', (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'GET') return http.Response(payload(), 200);
          return http.Response('{}', 200);
        }),
      );
      await pump(tester, client);

      await tester.tap(find.byType(Switch).first);
      await tester.pumpAndSettle();
      await tester.tap(find.text('Keep it on'));
      await tester.pumpAndSettle();

      expect(calls.where((c) => c.startsWith('POST')), isEmpty);
    });

    testWidgets('a refusal reads as admins-only, not as an error', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );
      await pump(tester, client);

      expect(find.text('Admins only'), findsOneWidget);
      expect(find.textContaining('core:admin'), findsOneWidget);
      // Not an error state: there is nothing to retry, and nothing is wrong.
      expect(find.text('Cannot reach the server'), findsNothing);
    });
  });
}
