/// The client on a **real Android device**, against a **real server** — the half
/// of the Android box a build cannot reach.
///
/// `flutter build apk` proves the app compiles. `flutter test` (the `client`
/// job's 150 tests) drives mocked wires on the host. Neither proves the thing a
/// scout actually needs: that the APK installs on a phone, launches, reaches the
/// server over the network, and signs in. #111 was that gap, and this file is the
/// close of it.
///
/// It runs **inside the app, on the device** (`flutter test integration_test/
/// app_test.dart -d emulator-5554`), so every assertion below is made by the real
/// client on the real platform — not by a host-side harness modelling it.
///
/// **It fails loudly when the server is absent.** A probe nobody runs, or one
/// that silently skips, is not evidence: with the base URL unreachable this file
/// reports a failing test naming the address, and never reaches a passing state.
/// There is no skip, no tolerance, and no mock fallback.
///
/// Configuration comes from `--dart-define`, so nothing here holds a credential:
///
///     flutter test integration_test/app_test.dart -d emulator-5554 \
///       --dart-define=ADJUTANT_DEVICE_BASE=http://10.0.2.2:8790 \
///       --dart-define=ADJUTANT_DEVICE_USER=client-harness-chief \
///       --dart-define=ADJUTANT_DEVICE_PASSWORD=...
///
/// `scripts/android-device-gate.sh` runs the whole gate — database, server, APK,
/// device — with those values, which is also how CI runs it.
library;

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/home_shell.dart';
import 'package:adjutant_client/screens/login_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:provider/provider.dart';

/// Where the server is, as seen from the device.
///
/// The default is the Android emulator's alias for its host's loopback, which is
/// where the gate script binds the server. A physical phone would pass the LAN
/// address instead — the value is configuration, not a constant of the test.
String get _base => const String.fromEnvironment(
      'ADJUTANT_DEVICE_BASE',
      defaultValue: 'http://10.0.2.2:8790',
    );

String get _user => const String.fromEnvironment(
      'ADJUTANT_DEVICE_USER',
      defaultValue: 'client-harness-chief',
    );

String get _password =>
    const String.fromEnvironment('ADJUTANT_DEVICE_PASSWORD');

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('the app on the device reaches the server it is pointed at',
      (tester) async {
    final api = ApiClient(baseUrl: _base);
    addTearDown(api.close);

    // 1. The device's own network path to the server. This is the assertion a
    //    host-side harness cannot make: it is the *device* that has to get
    //    there, through the emulator's NAT or the phone's wifi.
    final health = await api.send('GET', '/');
    expect(
      health,
      isA<Map>(),
      reason: 'GET $_base/ did not answer with JSON — the server is not '
          'reachable from the device (or something else holds that port)',
    );

    // 2. Sign in as a real user, through the app's own client, on the device.
    final token = await api.login(username: _user, password: _password);
    expect(token, isNotEmpty, reason: 'login returned no token');

    // 3. A read that only a real session can make — the roster the server
    //    stores. Its presence is what makes this a signed-in device, not a page
    //    that merely rendered.
    final members = await api.members();
    expect(
      members,
      isNotEmpty,
      reason: 'the roster is empty: the server is up but has no bootstrapped '
          'chief, so the gate is not exercising what it claims to',
    );
  });

  testWidgets('a scout signs in on the device, through the screens',
      (tester) async {
    final session = SessionState(client: ApiClient(baseUrl: _base));

    await tester.pumpWidget(
      ChangeNotifierProvider<SessionState>.value(
        value: session,
        child: const MaterialApp(home: LoginScreen()),
      ),
    );
    await tester.pumpAndSettle();

    // The form is the app's own, and it is pre-filled with the address the
    // session holds — so this is the first-run path a person walks.
    expect(find.text('Adjutant'), findsOneWidget);

    final fields = find.byType(TextFormField);
    expect(fields, findsNWidgets(3), reason: 'Server, Username, Password');
    await tester.enterText(fields.at(1), _user);
    await tester.enterText(fields.at(2), _password);
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(FilledButton, 'Sign In'));
    await tester.pumpAndSettle(const Duration(seconds: 2));

    // Signed in on the device: the shell is up, and the login form is gone.
    expect(
      find.byType(HomeShell),
      findsOneWidget,
      reason: 'the device never left the sign-in screen — look at the server '
          'log for the login attempt, and at the address the session holds',
    );
    expect(find.byType(LoginScreen), findsNothing);
    expect(find.text('Dashboard'), findsWidgets);
  });
}
