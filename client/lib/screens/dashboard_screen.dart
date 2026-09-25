import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Dashboard — a Monitor surface: glanceable state, not a marketing page.
class DashboardScreen extends StatefulWidget {
  const DashboardScreen({super.key});

  @override
  State<DashboardScreen> createState() => _DashboardScreenState();
}

class _DashboardScreenState extends State<DashboardScreen> {
  List<Map<String, dynamic>> _missions = const [];
  List<Map<String, dynamic>> _upcoming = const [];
  List<Map<String, dynamic>> _motions = const [];
  int? _memberCount;
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final session = context.read<SessionState>();
    setState(() {
      _loading = true;
      _error = null;
    });
    try {
      final missions = await session.cachedList('missions', session.api.missions);
      final upcoming = await session.cachedList('upcoming', session.api.upcomingEvents);
      final motions = await session.cachedList('motions', session.api.motions);

      // The roster is a count on the dashboard, so a failure here must not take
      // the whole screen down with it.
      List<Map<String, dynamic>> members = const [];
      try {
        members = await session.cachedList('members', session.api.members).then((c) => c.value);
      } on Object {
        members = const [];
      }

      if (!mounted) return;
      setState(() {
        _missions = missions.value;
        _upcoming = upcoming.value;
        _motions = motions.value;
        _memberCount = members.isEmpty ? null : members.length;
        _stale = missions.isStale || upcoming.isStale;
        _cachedAt = missions.cachedAt;
        _loading = false;
      });
    } on Object catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.toString();
        _loading = false;
      });
    }
  }

  int get _activeMissions => _missions
      .where((m) => const ['execution', 'in_progress', 'approved'].contains(
          field(m, ['stage', 'status']).toLowerCase()))
      .length;

  int get _openMotions => _motions
      .where((m) => const ['proposed', 'seconded', 'debate', 'voting']
          .contains(field(m, ['stage', 'status']).toLowerCase()))
      .length;

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const Center(child: CircularProgressIndicator());
    }
    if (_error != null && _missions.isEmpty) {
      return EmptyState(
        icon: Icons.cloud_off,
        title: 'Cannot reach the server',
        message: _error!,
        action: FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }

    final width = MediaQuery.sizeOf(context).width;
    final columns = width >= 1100 ? 4 : (width >= 700 ? 2 : 2);

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Expanded(
          child: RefreshIndicator(
            onRefresh: _load,
            child: ListView(
              padding: const EdgeInsets.all(AppSpacing.md),
              children: [
                _StatsRow(
                  columns: columns,
                  stats: [
                    _Stat('Active Missions', '$_activeMissions', AppColors.primary),
                    _Stat('Members', _memberCount?.toString() ?? '—', null),
                    _Stat('Upcoming Events', '${_upcoming.length}', AppColors.tertiary),
                    _Stat('Open Motions', '$_openMotions', AppColors.warning),
                  ],
                ),
                const SizedBox(height: AppSpacing.md),
                _RecentMissions(missions: _missions.take(5).toList(), onRetry: _load),
                const SizedBox(height: AppSpacing.md),
                _UpcomingEvents(events: _upcoming.take(4).toList()),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _Stat {
  const _Stat(this.label, this.value, this.color);
  final String label;
  final String value;
  final Color? color;
}

class _StatsRow extends StatelessWidget {
  const _StatsRow({required this.stats, required this.columns});

  final List<_Stat> stats;
  final int columns;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    // A fixed row height rather than an aspect ratio: an aspect ratio makes a
    // card's height depend on the phone's width, and on a narrow one the label
    // and the figure no longer fit inside their own card (the numbers are large
    // on purpose — they are read at a glance, outdoors). A stated extent is the
    // same on every phone and cannot overflow.
    return GridView(
      shrinkWrap: true,
      physics: const NeverScrollableScrollPhysics(),
      gridDelegate: SliverGridDelegateWithFixedCrossAxisCount(
        crossAxisCount: columns,
        mainAxisSpacing: AppSpacing.sm,
        crossAxisSpacing: AppSpacing.sm,
        mainAxisExtent: 96,
      ),
      children: [
        for (final s in stats)
          AppCard(
            padding: const EdgeInsets.all(AppSpacing.md),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(
                  s.label.toUpperCase(),
                  style: AppText.labelSmall.copyWith(color: scheme.outline, letterSpacing: 0.5),
                ),
                const SizedBox(height: 4),
                Text(
                  s.value,
                  style: AppText.displayMedium.copyWith(color: s.color),
                ),
              ],
            ),
          ),
      ],
    );
  }
}

class _RecentMissions extends StatelessWidget {
  const _RecentMissions({required this.missions, required this.onRetry});

  final List<Map<String, dynamic>> missions;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Recent Missions', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.md),
          if (missions.isEmpty)
            const EmptyState(
              icon: Icons.flag_outlined,
              title: 'No missions yet',
              message: 'Missions your troop proposes will appear here as they move through '
                  'the six-stage lifecycle.',
            )
          else
            for (final m in missions) ...[
              _MissionRow(mission: m),
              const SizedBox(height: AppSpacing.sm),
            ],
        ],
      ),
    );
  }
}

class _MissionRow extends StatelessWidget {
  const _MissionRow({required this.mission});

  final Map<String, dynamic> mission;

  @override
  Widget build(BuildContext context) {
    final episode = field(mission, ['lodge_name', 'location']);
    final dates = _dateRange(mission);
    final subtitle = [episode, dates].where((s) => s.isNotEmpty).join(' · ');
    return Container(
      padding: const EdgeInsets.all(AppSpacing.md),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(AppRadius.md),
        border: Border.all(color: Theme.of(context).colorScheme.outlineVariant),
      ),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  field(mission, ['title'], fallback: 'Untitled mission'),
                  style: AppText.titleMedium,
                ),
                if (subtitle.isNotEmpty) ...[
                  const SizedBox(height: 2),
                  Text(subtitle, style: AppText.bodySmall),
                ],
              ],
            ),
          ),
          const SizedBox(width: AppSpacing.sm),
          StatusBadge(field(mission, ['stage', 'status'], fallback: 'open')),
        ],
      ),
    );
  }
}

class _UpcomingEvents extends StatelessWidget {
  const _UpcomingEvents({required this.events});

  final List<Map<String, dynamic>> events;

  @override
  Widget build(BuildContext context) {
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Upcoming', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.md),
          if (events.isEmpty)
            const EmptyState(
              icon: Icons.event_available_outlined,
              title: 'Nothing scheduled',
              message: 'Meetings, campouts, and Congress will show up here.',
            )
          else
            for (final e in events) ...[
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.md),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Container(
                      width: 4,
                      height: 40,
                      margin: const EdgeInsets.only(top: 2, right: AppSpacing.md),
                      decoration: BoxDecoration(
                        color: Theme.of(context).colorScheme.primary,
                        borderRadius: BorderRadius.circular(2),
                      ),
                    ),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            field(e, ['title'], fallback: 'Untitled event'),
                            style: AppText.titleMedium,
                          ),
                          const SizedBox(height: 2),
                          Text(
                            [
                              formatRelativeDate(field(e, ['starts_at', 'start'])),
                              field(e, ['location']),
                            ].where((s) => s.isNotEmpty && s != '—').join(' · '),
                            style: AppText.bodySmall,
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
              ),
            ],
        ],
      ),
    );
  }
}

String _dateRange(Map<String, dynamic> m) {
  final start = field(m, ['starts_on', 'start_date']);
  final end = field(m, ['ends_on', 'end_date']);
  if (start.isEmpty && end.isEmpty) return '';
  if (end.isEmpty || end == start) return formatDate(start);
  return '${formatDate(start)} – ${formatDate(end)}';
}
