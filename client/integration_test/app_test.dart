/// The client on a **real Android device**, against a **real server** — the half
/// of the Android box that a build cannot reach.
///
/// `flutter build apk` proves the client compiles. `flutter test` (the `client`
/// job's 150 tests) drives mocked wires on the host. Neither proves the thing a
/// scout actually needs: that the APK installs on a phone, launches, reaches the
/// server over the network, and signs in. #111 was that gap, and this file closes
/// it.
///
/// Everything below runs **inside the app, on the device** (`flutter test
/// integration_test/app_test.dart -d emulator-5554`), so each assertion is made
/// by the real client on the real platform — never by a host-side harness
/// modelling it.
///
/// **It fails loudly when the server is absent.** A probe nobody runs, or one
/// that silently skips, is not evidence: with the base URL unreachable this file
/// reports a failing test naming the address, and never reaches a passing state.
/// No skip, no tolerance, no mock fallback.
///
/// The server side of that contract is `scripts/android-device-gate.sh`: it
/// creates the database, the identity **and the fixture this file asserts on**
/// (a lodge, a patrol, a scout). Read them together — the fixture is what makes
/// "the roster came from the server" mean something.
///
/// Configuration arrives as `--dart-define`, so nothing here holds a credential:
///
///     flutter test integration_test/app_test.dart -d emulator-5554 \
///       --dart-define=ADJUTANT_DEVICE_BASE=http://10.0.2.2:8790 \
///       --dart-define=ADJUTANT_DEVICE_USER=client-harness-chief \
///       --dart-define=ADJUTANT_DEVICE_PASSWORD=…
///       --dart-define=ADJUTANT_DEVICE_SCOUT=harness-scout
library;

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/main.dart';
import 'package:adjutant_client/screens/home_shell.dart';
import 'package:adjutant_client/screens/login_screen.dart';
import 'package:adjutant_client/screens/plugin_wizard_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

/// Where the server is, as seen from the device.
///
/// The default is the Android emulator's alias for its host's loopback, where the
/// gate script binds the server. A physical phone passes its LAN address instead —
/// this is configuration, not a constant of the test.
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

/// The scout the gate's script created through the API, before any of this ran.
String get _scout => const String.fromEnvironment(
      'ADJUTANT_DEVICE_SCOUT',
      defaultValue: 'harness-scout',
    );

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('the device reaches the server, and the server answers',
      (tester) async {
    final api = ApiClient(baseUrl: _base);
    addTearDown(api.close);

    // 1. The device's own network path. This is the assertion a host-side
    //    harness cannot make: it is the *device* that has to get there, through
    //    the emulator's NAT or the phone's wifi.
    final health = await api.send('GET', '/');
    expect(
      health,
      isA<Map>(),
      reason: 'GET $_base/ did not answer with JSON — the server is not '
          'reachable from the device (or something else holds that port)',
    );

    // 2. A real session, issued by the server to this device.
    final token = await api.login(username: _user, password: _password);
    expect(token, isNotEmpty, reason: 'login returned no token');

    final me = await api.me();
    expect(me['username'], _user,
        reason: 'the session the device holds belongs to somebody else');

    // 3. Server state, written by the server and read back through the product's
    //    own client. The gate's script created this scout before the test ran, so
    //    an empty roster means the gate is pointed at the wrong database — which
    //    is exactly the failure worth failing on.
    final members = await api.members();
    expect(
      members.map((m) => m['username'] ?? m['display_name']),
      contains(_scout),
      reason: 'the roster does not contain $_scout: the server is up, but the '
          'gate is not exercising the database its own script prepared',
    );
  });

  testWidgets('a scout signs in on the device, through the app\'s own screens',
      (tester) async {
    // The real root, not a screen in isolation: this is the first-run path —
    // boot, the sign-in form, and whatever the app does next.
    await tester.pumpWidget(const AdjutantApp());
    await tester.pumpAndSettle();

    expect(
      find.byType(LoginScreen),
      findsOneWidget,
      reason: 'a fresh install should start at the sign-in screen (the gate '
          'clears the app\'s data first; a restored session here means it did not)',
    );

    final fields = find.byType(TextFormField);
    expect(fields, findsNWidgets(3), reason: 'Server, Username, Password');

    // The Server field is the point of the first-run form: the app has no
    // default worth having, so the address is typed by hand here exactly as a
    // scout types it.
    await tester.enterText(fields.at(0), _base);
    await tester.enterText(fields.at(1), _user);
    await tester.enterText(fields.at(2), _password);
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(FilledButton, 'Sign In'));
    await tester.pumpAndSettle(const Duration(seconds: 3));

    // Signed in on the device. Two things prove it: the form is gone, and either
    // the shell or the first-run plugin prompt is up — the prompt is pushed over
    // the shell on a deployment that has recorded no plugin choice, which is
    // exactly what a fresh gate database is.
    expect(find.byType(LoginScreen), findsNothing,
        reason: 'the device never left the sign-in screen — check the server '
            'log for the login attempt and the address the form was given');
    expect(
      find.byType(HomeShell).evaluate().isNotEmpty ||
          find.byType(PluginWizardScreen).evaluate().isNotEmpty,
      isTrue,
      reason: 'signed in, but neither the shell nor the first-run prompt is up',
    );
  });
}
