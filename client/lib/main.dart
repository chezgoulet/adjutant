import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import 'screens/home_shell.dart';
import 'screens/login_screen.dart';
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

    return session.isAuthenticated ? const HomeShell() : const LoginScreen();
  }
}
