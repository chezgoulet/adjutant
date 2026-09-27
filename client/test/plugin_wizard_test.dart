import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/plugin_wizard_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';

/// The first-run choice (#90): what it preselects, what it refuses to offer, and
/// whose words a refusal arrives in.

http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

/// The rules payload exactly as the server sends it — the required reasons are the
/// server's own sentences, which is the point of shipping them.
const _requiredAuth =
    'authentication: disabling it locks every admin out of the server until a restart';
const _requiredMembership = 'the roster: every grant addresses a member it names';

Map<String, dynamic> _choicePayload({Map<String, dynamic>? choice}) => {
      'choice': choice,
      'required': [
        {'id': 'auth', 'why': _requiredAuth},
        {'id': 'membership', 'why': _requiredMembership},
      ],
      'dependencies': [
        {'dependent': 'store', 'dependency': 'stripe'},
      ],
    };

Map<String, dynamic> _pluginsPayload() => {
      'plugins': [
        {
          'id': 'auth',
          'name': 'Auth',
          'routes': 4,
          'permissions': ['core:admin', 'auth:read'],
          'enabled': true,
        },
        {'id': 'hello', 'name': 'Hello', 'routes': 1, 'permissions': [], 'enabled': true},
        {
          'id': 'membership',
          'name': 'Membership',
          'routes': 6,
          'permissions': ['members:read', 'members:write', 'core:admin'],
          'enabled': true,
        },
        {
          'id': 'store',
          'name': 'Store',
          'routes': 9,
          'permissions': ['store:read', 'store:write', 'store:admin', 'core:admin'],
          'enabled': true,
        },
        {
          'id': 'stripe',
          'name': 'Stripe',
          'routes': 2,
          'permissions': ['store:pay'],
          'enabled': false,
        },
      ],
    };

Widget _wrap(SessionState session, Widget child) =>
    ChangeNotifierProvider<SessionState>.value(
      value: session,
      child: MaterialApp(theme: AppTheme.light(), home: child),
    );

/// A viewport tall enough to build every row.
///
/// The screen is a `ListView`, so rows below the fold are never built and cannot
/// be found — a widget that exists on a phone can be invisible to a default
/// 800×600 test surface. Sizing the surface is the honest fix; scrolling
/// choreography in a test would be asserting about the test, not the screen.
void _roomForEveryRow(WidgetTester tester) {
  tester.view.physicalSize = const Size(1000, 2600);
  tester.view.devicePixelRatio = 1.0;
  addTearDown(tester.view.resetPhysicalSize);
  addTearDown(tester.view.resetDevicePixelRatio);
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('the choice endpoints', () {
    test('a choice is a GET of the payload that carries the rules too', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return _json(_choicePayload());
        }),
      );

      final payload = await client.pluginChoice();

      expect(calls, ['GET /api/plugins/choice']);
      expect(payload['choice'], isNull);
      // The reasons travel with the rule, so the screen repeats the server's own
      // sentence rather than a second copy that can drift from it.
      expect((payload['required'] as List).length, 2);
      expect(
        ((payload['required'] as List).first as Map)['why'],
        _requiredAuth,
      );
      expect((payload['dependencies'] as List).length, 1);
    });

    test('choosing sends the set as plugin_ids, not as a list of per-plugin calls',
        () async {
      final bodies = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          bodies.add(request.body);
          return _json({'enabled': ['auth', 'membership']});
        }),
      );

      final enabled = await client.setPluginChoice(['auth', 'membership']);

      expect(bodies.single, '{"plugin_ids":["auth","membership"]}');
      expect(enabled, ['auth', 'membership']);
    });
  });

  group('the wizard on first run', () {
    testWidgets('preselects only what cannot be off, and does not offer to turn it off',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/plugins/choice') {
            return _json(_choicePayload());
          }
          return _json(_pluginsPayload());
        }),
      );

      await tester.pumpWidget(
        _wrap(SessionState(client: client), const PluginWizardScreen(firstRun: true)),
      );
      await tester.pumpAndSettle();
      _roomForEveryRow(tester);
      await tester.pumpAndSettle();

      // Start minimal: the two required plugins, and nothing else.
      expect(find.text('2 of 5 on'), findsOneWidget);

      // Required plugins are shown with the server's own reason and no switch, so
      // the screen never offers a set the core would refuse.
      expect(find.text('Always on'), findsNWidgets(2));
      expect(find.text(_requiredAuth), findsOneWidget);
      expect(find.text(_requiredMembership), findsOneWidget);

      // The three that are a choice each have one.
      expect(find.byType(Switch), findsNWidgets(3));

      // And it says plainly what "off" costs, since that is the leader's question.
      expect(
        find.textContaining('does not delete anything'),
        findsOneWidget,
      );
    });

    testWidgets('a refusal is shown in the server\'s words', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/plugins/choice' && request.method == 'GET') {
            return _json(_choicePayload());
          }
          if (request.url.path == '/api/plugins') {
            return _json(_pluginsPayload());
          }
          // The core refusing the set, exactly as `plugin_choice::apply` words it.
          return _json(
            {'error': 'store needs stripe, which is not in the set'},
            400,
          );
        }),
      );

      await tester.pumpWidget(
        _wrap(SessionState(client: client), const PluginWizardScreen(firstRun: true)),
      );
      await tester.pumpAndSettle();
      _roomForEveryRow(tester);
      await tester.pumpAndSettle();

      // Turn `store` on while `stripe` stays off: a set the dependency rule rejects.
      final switches = find.byType(Switch);
      await tester.tap(switches.at(1)); // auth, hello, membership, store, stripe
      await tester.pumpAndSettle();

      // The screen warns from the pairs the server reported — the same rule, not a
      // second copy of it.
      expect(
        find.textContaining('store needs stripe'),
        findsOneWidget,
      );

      await tester.tap(find.widgetWithText(FilledButton, 'Save'));
      await tester.pumpAndSettle();

      // And still shows the server's sentence when it refuses.
      expect(find.text('store needs stripe, which is not in the set'), findsOneWidget);
    });

    /// Issue #122. A skip is not a decision: it must record nothing, so the
    /// deployment keeps running what is on disk — which is exactly what the
    /// screen's own copy promises before the tap ("No choice has been recorded,
    /// so this deployment is running everything on disk"). Before this, the
    /// button called `_save`, so an operator who skipped to look around silently
    /// switched most of the deployment off.
    testWidgets('skipping records nothing and leaves without a choice',
        (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.url.path == '/api/plugins/choice') {
            return _json(_choicePayload());
          }
          return _json(_pluginsPayload());
        }),
      );

      // Pushed the way the Plugins screen opens it, so the skip's `pop` is a
      // real navigation rather than a pop of the only route.
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>.value(
          value: SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: Builder(
              builder: (context) => Scaffold(
                body: Center(
                  child: TextButton(
                    onPressed: () => Navigator.of(context).push(
                      MaterialPageRoute(
                        builder: (_) => const PluginWizardScreen(firstRun: true),
                      ),
                    ),
                    child: const Text('open the wizard'),
                  ),
                ),
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('open the wizard'));
      await tester.pumpAndSettle();
      _roomForEveryRow(tester);
      await tester.pumpAndSettle();
      expect(find.byType(PluginWizardScreen), findsOneWidget);

      // Only the tap's own requests count; the screen's load has already been
      // paid for above.
      calls.clear();
      await tester.tap(find.text('Skip for now'));
      await tester.pumpAndSettle();

      expect(
        calls,
        isEmpty,
        reason: 'a skip must send nothing: recording the minimal set is a '
            'choice the operator did not make (issue #122)',
      );
      // And it leaves, so the caller does not sit on a wizard whose question the
      // operator declined.
      expect(find.byType(PluginWizardScreen), findsNothing);
    });
  });
}
