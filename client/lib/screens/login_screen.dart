import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../api/server_address.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';

/// Sign in, and point the app at a server.
///
/// The server field is deliberately on the login screen rather than buried in
/// settings: Adjutant is self-hosted, so "which box?" is a first-run question,
/// not an advanced one.
class LoginScreen extends StatefulWidget {
  const LoginScreen({super.key});

  @override
  State<LoginScreen> createState() => _LoginScreenState();
}

class _LoginScreenState extends State<LoginScreen> {
  final _formKey = GlobalKey<FormState>();
  late final TextEditingController _baseUrl;
  final _username = TextEditingController();
  final _password = TextEditingController();

  bool _busy = false;
  String? _error;

  @override
  void initState() {
    super.initState();
    _baseUrl = TextEditingController(
      text: context.read<SessionState>().api.baseUrl,
    );
  }

  @override
  void dispose() {
    _baseUrl.dispose();
    _username.dispose();
    _password.dispose();
    super.dispose();
  }

  Future<void> _submit() async {
    if (!(_formKey.currentState?.validate() ?? false)) return;

    // Send the canonical form of what was typed, and show it back, so the field
    // stops disagreeing with the address actually in use. A bare host becomes
    // `https://host/`, which is also what gets saved and restored next launch.
    final address = normaliseServerAddress(_baseUrl.text);
    if (address == null) return;
    if (address != _baseUrl.text) {
      _baseUrl.text = address;
    }

    setState(() {
      _busy = true;
      _error = null;
    });
    final session = context.read<SessionState>();
    try {
      await session.signIn(
        baseUrl: address,
        username: _username.text.trim(),
        password: _password.text,
      );
      // The shell is chosen by auth state in main.dart; nothing to navigate.
    } on ApiException catch (e) {
      setState(() => _error = e.statusCode == 400 ? 'Invalid credentials' : e.message);
    } on OfflineException {
      setState(() => _error = 'Cannot reach the server. Check the address and your connection.');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final wide = MediaQuery.sizeOf(context).width >= 720;

    return Scaffold(
      body: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(AppSpacing.lg),
          child: ConstrainedBox(
            constraints: BoxConstraints(maxWidth: wide ? 420 : 400),
            child: Form(
              key: _formKey,
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  // The mark: fleur-de-lis — scouting and Québécois identity.
                  Center(
                    child: Container(
                      width: 72,
                      height: 72,
                      decoration: BoxDecoration(
                        color: scheme.primary,
                        borderRadius: BorderRadius.circular(AppRadius.lg),
                      ),
                      alignment: Alignment.center,
                      child: const Text('⚜️', style: TextStyle(fontSize: 36)),
                    ),
                  ),
                  const SizedBox(height: AppSpacing.md),
                  Text('Adjutant', style: AppText.displayLarge, textAlign: TextAlign.center),
                  const SizedBox(height: AppSpacing.xs),
                  Text(
                    'Sign in to your troop',
                    style: AppText.bodyMedium.copyWith(color: scheme.outline),
                    textAlign: TextAlign.center,
                  ),
                  const SizedBox(height: AppSpacing.xl),

                  TextFormField(
                    controller: _baseUrl,
                    decoration: InputDecoration(
                      labelText: 'Server',
                      helperText: cleartextAllowed
                          ? 'The address of your troop’s Adjutant server'
                          : 'The https address of your troop’s Adjutant server',
                    ),
                    keyboardType: TextInputType.url,
                    autocorrect: false,
                    // The same rule the client actually applies, so the field
                    // cannot accept an address the app will refuse to use.
                    validator: (v) => serverAddressProblem(v ?? ''),
                  ),
                  const SizedBox(height: AppSpacing.md),
                  TextFormField(
                    controller: _username,
                    decoration: const InputDecoration(labelText: 'Username'),
                    autocorrect: false,
                    textInputAction: TextInputAction.next,
                    validator: (v) =>
                        (v == null || v.trim().isEmpty) ? 'Enter your username' : null,
                  ),
                  const SizedBox(height: AppSpacing.md),
                  TextFormField(
                    controller: _password,
                    decoration: const InputDecoration(labelText: 'Password'),
                    obscureText: true,
                    onFieldSubmitted: (_) => _submit(),
                    validator: (v) =>
                        (v == null || v.isEmpty) ? 'Enter your password' : null,
                  ),

                  if (_error != null) ...[
                    const SizedBox(height: AppSpacing.md),
                    Container(
                      padding: const EdgeInsets.all(AppSpacing.md),
                      decoration: BoxDecoration(
                        color: AppColors.statusContainer('rejected', Theme.of(context).brightness),
                        borderRadius: BorderRadius.circular(AppRadius.sm),
                      ),
                      child: Row(
                        children: [
                          Icon(Icons.error_outline,
                              size: 18,
                              color: AppColors.statusForeground(
                                  'rejected', Theme.of(context).brightness)),
                          const SizedBox(width: AppSpacing.sm),
                          Expanded(
                            child: Text(
                              _error!,
                              style: AppText.bodyMedium.copyWith(
                                color: AppColors.statusForeground(
                                    'rejected', Theme.of(context).brightness),
                              ),
                            ),
                          ),
                        ],
                      ),
                    ),
                  ],

                  const SizedBox(height: AppSpacing.lg),
                  FilledButton(
                    onPressed: _busy ? null : _submit,
                    child: _busy
                        ? const SizedBox(
                            width: 20,
                            height: 20,
                            child: CircularProgressIndicator(strokeWidth: 2),
                          )
                        : const Text('Sign In'),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
