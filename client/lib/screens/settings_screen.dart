import 'package:flutter/material.dart';

import '../theme/app_theme.dart';
import 'dues_screen.dart';
import 'plugins_screen.dart';

/// Settings — where you go when something needs changing.
///
/// A plain list of submenus rather than a fifth navigation destination per
/// setting: the shell's destinations are the troop's daily work (missions,
/// announcement inbox, calendar, members, dashboard), and this is the drawer
/// behind them. Plugins is the first entry, not the only one forever.
class SettingsScreen extends StatelessWidget {
  const SettingsScreen({super.key});

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return ListView(
      children: [
        _SectionHeader('Mine'),
        ListTile(
          leading: const Icon(Icons.receipt_long_outlined),
          title: const Text('Dues'),
          subtitle: const Text(
            'What I owe this year, and reporting my sliding-scale tier',
          ),
          trailing: const Icon(Icons.chevron_right),
          onTap: () => Navigator.of(context).push(
            MaterialPageRoute<void>(builder: (_) => const DuesScreen()),
          ),
        ),
        const Divider(height: 1),
        _SectionHeader('The House'),
        ListTile(
          leading: const Icon(Icons.extension_outlined),
          title: const Text('Plugins'),
          subtitle: const Text('What this troop runs, and what is switched off'),
          trailing: const Icon(Icons.chevron_right),
          onTap: () => Navigator.of(context).push(
            MaterialPageRoute<void>(builder: (_) => const PluginsScreen()),
          ),
        ),
        const Divider(height: 1),
        Padding(
          padding: const EdgeInsets.all(AppSpacing.md),
          child: Text(
            'More settings arrive with the features that need them. Nothing is '
            'here that the server does not enforce — a setting this app offers '
            'is one the core will honour for the people entitled to it.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ),
      ],
    );
  }
}

class _SectionHeader extends StatelessWidget {
  const _SectionHeader(this.label);

  final String label;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(
        left: AppSpacing.md,
        right: AppSpacing.md,
        top: AppSpacing.lg,
        bottom: AppSpacing.sm,
      ),
      child: Text(
        label.toUpperCase(),
        style: AppText.labelMedium.copyWith(
          color: scheme.outline,
          letterSpacing: 0.8,
        ),
      ),
    );
  }
}
