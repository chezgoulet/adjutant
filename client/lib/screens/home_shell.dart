import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import 'calendar_screen.dart';
import 'dashboard_screen.dart';
import 'members_screen.dart';
import 'missions_screen.dart';

/// The shell every screen lives in.
///
/// One composition, three layouts, chosen by width (design language §5):
/// bottom bar on a phone, rail on a tablet, sidebar on a desktop. The
/// destinations are the same, so nothing is lost moving between them.
class HomeShell extends StatefulWidget {
  const HomeShell({super.key});

  @override
  State<HomeShell> createState() => _HomeShellState();
}

class _HomeShellState extends State<HomeShell> {
  int _index = 0;

  static const _destinations = [
    _Destination('Dashboard', Icons.dashboard_outlined, Icons.dashboard),
    _Destination('Missions', Icons.flag_outlined, Icons.flag),
    _Destination('Calendar', Icons.calendar_month_outlined, Icons.calendar_month),
    _Destination('Members', Icons.groups_outlined, Icons.groups),
  ];

  Widget _body() => switch (_index) {
        0 => const DashboardScreen(),
        1 => const MissionsScreen(),
        2 => const CalendarScreen(),
        _ => const MembersScreen(),
      };

  @override
  Widget build(BuildContext context) {
    final width = MediaQuery.sizeOf(context).width;
    final session = context.watch<SessionState>();
    final title = _destinations[_index].label;

    final actions = <Widget>[
      if (session.offline)
        Padding(
          padding: const EdgeInsets.only(right: AppSpacing.sm),
          child: Tooltip(
            message: 'Offline',
            child: Icon(Icons.cloud_off, color: AppColors.warning, size: 20),
          ),
        ),
      PopupMenuButton<String>(
        tooltip: 'Account',
        onSelected: (value) {
          if (value == 'signout') context.read<SessionState>().signOut();
        },
        itemBuilder: (context) => [
          PopupMenuItem(enabled: false, child: Text(session.displayName)),
          if (session.roles.isNotEmpty)
            PopupMenuItem(
              enabled: false,
              child: Text(
                session.roles.join(', '),
                style: AppText.bodySmall,
              ),
            ),
          const PopupMenuDivider(),
          const PopupMenuItem(value: 'signout', child: Text('Sign out')),
        ],
        child: const Padding(
          padding: EdgeInsets.symmetric(horizontal: AppSpacing.md),
          child: Icon(Icons.account_circle_outlined),
        ),
      ),
    ];

    // Compact — phone. Bottom bar.
    if (width < 600) {
      return Scaffold(
        appBar: AppBar(title: Text(title), actions: actions),
        body: _body(),
        bottomNavigationBar: NavigationBar(
          selectedIndex: _index,
          onDestinationSelected: (i) => setState(() => _index = i),
          destinations: [
            for (final d in _destinations)
              NavigationDestination(
                icon: Icon(d.icon),
                selectedIcon: Icon(d.selectedIcon),
                label: d.label,
              ),
          ],
        ),
      );
    }

    // Medium — tablet. Navigation rail.
    if (width < 840) {
      return Scaffold(
        appBar: AppBar(title: Text(title), actions: actions),
        body: Row(
          children: [
            NavigationRail(
              selectedIndex: _index,
              onDestinationSelected: (i) => setState(() => _index = i),
              labelType: NavigationRailLabelType.all,
              destinations: [
                for (final d in _destinations)
                  NavigationRailDestination(
                    icon: Icon(d.icon),
                    selectedIcon: Icon(d.selectedIcon),
                    label: Text(d.label),
                  ),
              ],
            ),
            const VerticalDivider(width: 1),
            Expanded(child: _body()),
          ],
        ),
      );
    }

    // Expanded — desktop. Sidebar with the mark.
    return Scaffold(
      body: Row(
        children: [
          _Sidebar(
            index: _index,
            onSelected: (i) => setState(() => _index = i),
            destinations: _destinations,
            displayName: session.displayName,
          ),
          const VerticalDivider(width: 1),
          Expanded(
            child: Scaffold(
              appBar: AppBar(title: Text(title), actions: actions),
              body: _body(),
            ),
          ),
        ],
      ),
    );
  }
}

class _Destination {
  const _Destination(this.label, this.icon, this.selectedIcon);

  final String label;
  final IconData icon;
  final IconData selectedIcon;
}

class _Sidebar extends StatelessWidget {
  const _Sidebar({
    required this.index,
    required this.onSelected,
    required this.destinations,
    required this.displayName,
  });

  final int index;
  final ValueChanged<int> onSelected;
  final List<_Destination> destinations;
  final String displayName;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      width: 260,
      color: scheme.surfaceContainerHighest.withValues(alpha: 0.4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.all(AppSpacing.md),
            child: Row(
              children: [
                Container(
                  width: 32,
                  height: 32,
                  decoration: BoxDecoration(
                    color: scheme.primary,
                    borderRadius: BorderRadius.circular(AppRadius.sm),
                  ),
                  alignment: Alignment.center,
                  child: const Text('⚜️', style: TextStyle(fontSize: 16)),
                ),
                const SizedBox(width: AppSpacing.sm),
                Text('Adjutant', style: AppText.titleLarge),
              ],
            ),
          ),
          const SizedBox(height: AppSpacing.sm),
          for (var i = 0; i < destinations.length; i++)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: AppSpacing.sm, vertical: 2),
              child: Material(
                color: i == index ? scheme.primaryContainer : Colors.transparent,
                borderRadius: BorderRadius.circular(AppRadius.sm),
                child: InkWell(
                  onTap: () => onSelected(i),
                  borderRadius: BorderRadius.circular(AppRadius.sm),
                  child: Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: AppSpacing.md,
                      vertical: 12,
                    ),
                    child: Row(
                      children: [
                        Icon(
                          i == index ? destinations[i].selectedIcon : destinations[i].icon,
                          size: 20,
                          color: i == index
                              ? scheme.onPrimaryContainer
                              : scheme.onSurface.withValues(alpha: 0.7),
                        ),
                        const SizedBox(width: AppSpacing.md),
                        Text(
                          destinations[i].label,
                          style: AppText.bodyLarge.copyWith(
                            color: i == index
                                ? scheme.onPrimaryContainer
                                : scheme.onSurface.withValues(alpha: 0.8),
                            fontWeight: i == index ? FontWeight.w500 : FontWeight.w400,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          const Spacer(),
          Padding(
            padding: const EdgeInsets.all(AppSpacing.md),
            child: Text(
              displayName,
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ),
        ],
      ),
    );
  }
}
