import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import 'announcements_screen.dart';
import 'calendar_screen.dart';
import 'dashboard_screen.dart';
import 'equipment_screen.dart';
import 'members_screen.dart';
import 'missions_screen.dart';
import 'settings_screen.dart';
import 'store_screen.dart';

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

  /// Where the announcements inbox sits in the destinations. It is here rather
  /// than under Settings because it is daily work: the notice that tonight's
  /// meeting moved is the first thing a scout needs, and the badge beside it is
  /// the only reason to open the app at all on some days.
  static const _inbox = 1;

  static const _destinations = [
    _Destination('Dashboard', Icons.dashboard_outlined, Icons.dashboard),
    _Destination('Inbox', Icons.inbox_outlined, Icons.inbox),
    _Destination('Missions', Icons.flag_outlined, Icons.flag),
    _Destination('Calendar', Icons.calendar_month_outlined, Icons.calendar_month),
    // The shop is ordinary troop work, not an administrative setting: a new
    // scout needs to find the uniform they have just been told to buy the same
    // week they join. Its operator surfaces (the unsettled worklist, adding an
    // item, the comp list) live behind Settings instead — see settings_screen.
    _Destination('Shop', Icons.storefront_outlined, Icons.storefront),
    // Equipment is the same kind of work — a scout checking a tent out for a
    // weekend is doing the thing the plugin exists for, not administering it.
    // Its quartermaster surfaces (maintenance, retirement, replacement) are not
    // in the client yet; what a scout needs is what is here.
    _Destination('Equipment', Icons.backpack_outlined, Icons.backpack),
    _Destination('Members', Icons.groups_outlined, Icons.groups),
    // Settings last: the daily work comes first, and the drawer behind it is
    // where you go when something needs changing. Plugins lives inside it.
    _Destination('Settings', Icons.settings_outlined, Icons.settings),
  ];

  @override
  void initState() {
    super.initState();
    // The badge is the server's count, read once when the shell appears. It is
    // cached like every other read, so a scout in the woods still sees the last
    // number the server gave rather than an empty badge that means nothing.
    context.read<SessionState>().refreshAnnouncementBadge();
  }

  Widget _body() => switch (_index) {
        0 => const DashboardScreen(),
        1 => const AnnouncementsScreen(),
        2 => const MissionsScreen(),
        3 => const CalendarScreen(),
        4 => const StoreScreen(),
        5 => const EquipmentScreen(),
        6 => const MembersScreen(),
        _ => const SettingsScreen(),
      };

  /// A destination's icon, carrying the unread count when it is the inbox.
  ///
  /// The badge is red only when an *unread urgent* announcement exists — that is
  /// the category that exists to interrupt, and a count that looks the same
  /// either way wastes it.
  Widget _iconFor(
    int i, {
    required bool selected,
    required int unread,
    required bool urgent,
  }) {
    final destination = _destinations[i];
    final icon = Icon(
      selected ? destination.selectedIcon : destination.icon,
    );
    if (i != _inbox || unread <= 0) return icon;
    return Badge(
      backgroundColor: urgent ? AppColors.error : null,
      label: Text(unread > 99 ? '99+' : '$unread'),
      child: icon,
    );
  }

  @override
  Widget build(BuildContext context) {
    final width = MediaQuery.sizeOf(context).width;
    final session = context.watch<SessionState>();
    final title = _destinations[_index].label;
    final unread = session.announcementUnread;
    final urgent = session.announcementUrgent;

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
            for (var i = 0; i < _destinations.length; i++)
              NavigationDestination(
                icon: _iconFor(i, selected: false, unread: unread, urgent: urgent),
                selectedIcon:
                    _iconFor(i, selected: true, unread: unread, urgent: urgent),
                label: _destinations[i].label,
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
                for (var i = 0; i < _destinations.length; i++)
                  NavigationRailDestination(
                    icon: _iconFor(i, selected: false, unread: unread, urgent: urgent),
                    selectedIcon:
                        _iconFor(i, selected: true, unread: unread, urgent: urgent),
                    label: Text(_destinations[i].label),
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
            badgeIndex: _inbox,
            unread: unread,
            urgent: urgent,
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
    required this.badgeIndex,
    required this.unread,
    required this.urgent,
  });

  final int index;
  final ValueChanged<int> onSelected;
  final List<_Destination> destinations;
  final String displayName;

  /// Which destination carries the unread count, and how many there are.
  final int badgeIndex;
  final int unread;
  final bool urgent;

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
                        if (i == badgeIndex && unread > 0)
                          Badge(
                            backgroundColor: urgent ? AppColors.error : null,
                            label: Text(unread > 99 ? '99+' : '$unread'),
                            child: Icon(
                              i == index
                                  ? destinations[i].selectedIcon
                                  : destinations[i].icon,
                              size: 20,
                            ),
                          )
                        else
                          Icon(
                            i == index
                                ? destinations[i].selectedIcon
                                : destinations[i].icon,
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
