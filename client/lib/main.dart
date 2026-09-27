import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import 'screens/home_shell.dart';
import 'screens/login_screen.dart';
import 'screens/plugin_wizard_screen.dart';
import 'state/session.dart';
import 'theme/app_theme.dart';

/// Adjutant — administration for a democratic scout troop.
///
/// The client is a renderer for server state. It holds no domain logic that the
/// server does not authorise: every action is an ordinary API call subject to
/// the route gate, and hiding a control is convenience, never security.
void main() {
  runApp(const AdjutantApp());
}

class AdjutantApp extends StatelessWidget {
  const AdjutantApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider(
      create: (_) => SessionState()..boot(),
      child: MaterialApp(
        title: 'Adjutant',
        debugShowCheckedModeBanner: false,
        theme: AppTheme.light(),
        darkTheme: AppTheme.dark(),
        themeMode: ThemeMode.system,
        home: const _Root(),
      ),
    );
  }
}

class _Root extends StatelessWidget {
  const _Root();

  @override
  Widget build(BuildContext context) {
    final session = context.watch<SessionState>();

    if (session.booting) {
      return const Scaffold(
        body: Center(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text('⚜️', style: TextStyle(fontSize: 40)),
              SizedBox(height: AppSpacing.md),
              SizedBox(
                width: 24,
                height: 24,
                child: CircularProgressIndicator(strokeWidth: 2),
              ),
            ],
          ),
        ),
      );
    }

    return session.isAuthenticated ? const _FirstRunGate() : const LoginScreen();
  }
}

/// The first-run prompt (#90).
///
/// Asks the *server*, once per session, whether an operator has chosen what this
/// deployment runs — because that is the only place the answer can live: a flag on
/// the device would re-prompt the second admin and could not be set by a scripted
/// install.
///
/// Three things it must never do, and does not:
///
/// * **Block someone who is not an admin.** The choice needs `core:admin`, and a
///   403 is that person's answer, not a gate on the rest of the app.
/// * **Hold up the app when the server is unreachable.** An offline start shows
///   the troop's screens; a prompt is not worth an empty screen in the woods.
/// * **Ask twice.** Once per session, and re-enterable from Plugins afterwards, so
///   a skipped prompt is a deferred decision rather than a lost one.
class _FirstRunGate extends StatefulWidget {
  const _FirstRunGate();

  @override
  State<_FirstRunGate> createState() => _FirstRunGateState();
}

class _FirstRunGateState extends State<_FirstRunGate> {
  bool _asked = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (_asked) return;
    _asked = true;
    WidgetsBinding.instance.addPostFrameCallback((_) => _ask());
  }

  Future<void> _ask() async {
    if (!mounted) return;
    final api = context.read<SessionState>().api;
    try {
      final payload = await api.pluginChoice();
      if (!mounted) return;
      if (payload['choice'] == null) {
        await Navigator.of(context).push(
          MaterialPageRoute(
            builder: (_) => const PluginWizardScreen(firstRun: true),
          ),
        );
      }
    } catch (_) {
      // Offline, or not an admin. Neither is a reason to hold up the app, and
      // Plugins still offers the same screen.
    }
  }

  @override
  Widget build(BuildContext context) => const HomeShell();
}
