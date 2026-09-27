/// A 4xx is the server explaining itself — it is not an offline condition.
///
/// The bug this pins down: with the announcements plugin switched off, the
/// server answered `GET /api/announcements/announcements` with
/// `404 {"error":"route not found"}`, and the Inbox rendered "Cannot reach the
/// server" with a Retry button — the same screen it shows when there is no
/// network at all. Retry cannot fix a route that does not exist, and the reader
/// was told the one thing that was not true.
///
/// These tests drive real screens over a mock `http.Client` and assert on the
/// rendered text: the server's own sentence, and no Retry where the answer
/// cannot change. `client/test/widget_test.dart` is deliberately untouched —
/// these live in their own file so two lanes can work without rebasing each
/// other.
library;

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/announcements_screen.dart';
import 'package:adjutant_client/screens/members_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:adjutant_client/widgets/common.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// The answer the phone actually got: a route this deployment does not run,
/// because the plugin that owns it is switched off.
const _routeNotFound = '{"error":"route not found"}';

/// The sentence the server wrote in place of a route — kept verbatim.
const _serverSentence = 'route not found';

/// A server that answers every request with one status and one body.
ApiClient _answering(int status, String body) => ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient(
        (_) async => http.Response(
          body,
          status,
          headers: {'content-type': 'application/json; charset=utf-8'},
        ),
      ),
    );

/// A server that is not there at all: the transport fails before any answer.
ApiClient _unreachable() => ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((_) async => throw http.ClientException('no route to host')),
    );

Future<void> _pump(WidgetTester tester, ApiClient client, Widget screen) async {
  await tester.pumpWidget(
    ChangeNotifierProvider<SessionState>(
      create: (_) => SessionState(client: client),
      child: MaterialApp(
        theme: AppTheme.light(),
        home: Scaffold(body: screen),
      ),
    ),
  );
  await tester.pumpAndSettle();
}

void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  group('the screen', () {
    testWidgets('renders a 404 as the server\'s own sentence, not as offline',
        (tester) async {
      await _pump(
        tester,
        _answering(404, _routeNotFound),
        const AnnouncementsScreen(),
      );

      // The bug this test exists for: the server was reached and answered, so
      // the offline screen — its title, and its Retry — must not be what the
      // reader sees.
      expect(find.text('Cannot reach the server'), findsNothing);
      expect(find.text('Retry'), findsNothing);
      // The server's words, verbatim — the message is *its* explanation.
      expect(find.text(_serverSentence), findsOneWidget);
      // And it is named as what it is: a route this deployment does not answer.
      expect(find.text('Not found on this server'), findsOneWidget);
    });

    testWidgets('a 4xx that is not 404 is the server refusing, also without Retry',
        (tester) async {
      await _pump(
        tester,
        _answering(409, '{"error":"no assessment is open to report against"}'),
        const AnnouncementsScreen(),
      );

      expect(find.text('Cannot reach the server'), findsNothing);
      expect(find.text('Retry'), findsNothing);
      expect(find.text('no assessment is open to report against'), findsOneWidget);
      expect(find.text('The server refused this request'), findsOneWidget);
    });

    testWidgets('a 5xx keeps its own wording and keeps Retry — it may work again',
        (tester) async {
      await _pump(
        tester,
        _answering(500, '{"error":"the plugin failed to load"}'),
        const AnnouncementsScreen(),
      );

      expect(find.text('Cannot reach the server'), findsNothing);
      expect(find.text('the plugin failed to load'), findsOneWidget);
      expect(find.text('The server could not answer'), findsOneWidget);
      // A 5xx is the server failing to answer something it does run, so trying
      // again is not a lie.
      expect(find.text('Retry'), findsOneWidget);
    });

    testWidgets('the roster renders a 404 as not-found, not as offline',
        (tester) async {
      await _pump(
        tester,
        _answering(404, _routeNotFound),
        const MembersScreen(),
      );

      expect(find.text('Cannot reach the server'), findsNothing);
      expect(find.text('Retry'), findsNothing);
      expect(find.text('Not found on this server'), findsOneWidget);
      expect(find.text(_serverSentence), findsOneWidget);
    });
  });

  group('the one classifier every screen uses', () {
    test('only a transport failure reads as offline, and only it offers Retry',
        () async {
      // Nothing came back: the offline case, and the only retryable one.
      final offline = describeFailure(
        'ClientException: no route to host',
        null,
      );
      expect(offline.title, 'Cannot reach the server');
      expect(offline.retryable, isTrue);

      // The server answered 404: not offline, and nothing to retry.
      final missing = describeFailure(_serverSentence, 404);
      expect(missing.title, isNot('Cannot reach the server'));
      expect(missing.message, _serverSentence);
      expect(missing.retryable, isFalse);

      // A named record missing says what is missing, in the screen's words.
      expect(describeFailure(_serverSentence, 404, missingTitle: 'No such item').title,
          'No such item');

      // Any other 4xx is the server explaining a refusal.
      final refused = describeFailure('forbidden by policy', 418);
      expect(refused.message, 'forbidden by policy');
      expect(refused.retryable, isFalse);

      // A 5xx may answer differently next time, so Retry stays.
      expect(describeFailure('boom', 503).retryable, isTrue);

      // The ApiClient distinguishes these two for us; nothing here re-derives
      // that from a string, and this is the assertion that ties the two
      // together: a real 404 carries a status, a real transport failure does
      // not.
      await expectLater(
        _answering(404, _routeNotFound).members(),
        throwsA(isA<ApiException>().having((e) => e.statusCode, 'status', 404)),
      );
      await expectLater(
        _unreachable().members(),
        throwsA(isA<OfflineException>()),
      );
    });
  });
}
