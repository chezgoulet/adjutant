import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Missions — Operate surface. The six-stage lifecycle, filterable by stage.
class MissionsScreen extends StatefulWidget {
  const MissionsScreen({super.key});

  @override
  State<MissionsScreen> createState() => _MissionsScreenState();
}

class _MissionsScreenState extends State<MissionsScreen> {
  List<Map<String, dynamic>> _missions = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  String _filter = 'all';

  static const _stages = [
    'all',
    'request',
    'review',
    'approved',
    'execution',
    'debrief',
    'report',
  ];

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
      final cached = await session.cachedList('missions', session.api.missions);
      if (!mounted) return;
      setState(() {
        _missions = cached.value;
        _stale = cached.isStale;
        _cachedAt = cached.cachedAt;
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

  List<Map<String, dynamic>> get _visible => _filter == 'all'
      ? _missions
      : _missions
          .where((m) => field(m, ['stage', 'status']).toLowerCase() == _filter)
          .toList();

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        SizedBox(
          height: 48,
          child: ListView(
            scrollDirection: Axis.horizontal,
            padding: const EdgeInsets.symmetric(horizontal: AppSpacing.md),
            children: [
              for (final stage in _stages)
                Padding(
                  padding: const EdgeInsets.only(right: AppSpacing.sm),
                  child: Center(
                    child: ChoiceChip(
                      label: Text(stage == 'all' ? 'All' : _stageLabel(stage)),
                      selected: _filter == stage,
                      onSelected: (_) => setState(() => _filter = stage),
                    ),
                  ),
                ),
            ],
          ),
        ),
        Expanded(
          child: _error != null && _missions.isEmpty
              ? EmptyState(
                  icon: Icons.cloud_off,
                  title: 'Cannot reach the server',
                  message: _error!,
                  action: FilledButton(onPressed: _load, child: const Text('Retry')),
                )
              : _visible.isEmpty
                  ? const EmptyState(
                      icon: Icons.flag_outlined,
                      title: 'No missions here',
                      message: 'Missions in this stage will appear here. Propose one to get '
                          'started — a mission begins as a request and moves through '
                          'review, approval, execution, debrief, and report.',
                    )
                  : RefreshIndicator(
                      onRefresh: _load,
                      child: ListView.separated(
                        padding: const EdgeInsets.all(AppSpacing.md),
                        itemCount: _visible.length,
                        separatorBuilder: (_, _) => const SizedBox(height: AppSpacing.sm),
                        itemBuilder: (context, i) => _MissionCard(
                          mission: _visible[i],
                          onOpen: () => _openDetail(_visible[i]),
                        ),
                      ),
                    ),
        ),
      ],
    );
  }

  void _openDetail(Map<String, dynamic> mission) {
    Navigator.of(context).push(
      MaterialPageRoute(builder: (_) => MissionDetailScreen(mission: mission)),
    );
  }

  static String _stageLabel(String stage) => switch (stage) {
        'request' => 'Requested',
        'review' => 'In Review',
        'approved' => 'Approved',
        'execution' => 'In Progress',
        'debrief' => 'Debrief',
        'report' => 'Report',
        _ => stage,
      };
}

class _MissionCard extends StatelessWidget {
  const _MissionCard({required this.mission, required this.onOpen});

  final Map<String, dynamic> mission;
  final VoidCallback onOpen;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final purpose = field(mission, ['purpose', 'description']);
    return AppCard(
      onTap: onOpen,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  field(mission, ['title'], fallback: 'Untitled mission'),
                  style: AppText.titleLarge,
                ),
              ),
              const SizedBox(width: AppSpacing.sm),
              StatusBadge(field(mission, ['stage', 'status'], fallback: 'open')),
            ],
          ),
          if (purpose.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              purpose,
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
              style: AppText.bodyMedium.copyWith(color: scheme.onSurface.withValues(alpha: 0.75)),
            ),
          ],
          const SizedBox(height: AppSpacing.sm),
          Wrap(
            spacing: AppSpacing.md,
            runSpacing: 4,
            children: [
              if (field(mission, ['lodge_name']).isNotEmpty)
                _Meta(Icons.home_outlined, field(mission, ['lodge_name'])),
              if (field(mission, ['location']).isNotEmpty)
                _Meta(Icons.place_outlined, field(mission, ['location'])),
              if (_dates(mission).isNotEmpty) _Meta(Icons.event_outlined, _dates(mission)),
              if (field(mission, ['service_hours']) != '0' &&
                  field(mission, ['service_hours']) != '0.0')
                _Meta(Icons.schedule, '${field(mission, ['service_hours'])} h'),
            ],
          ),
        ],
      ),
    );
  }

  static String _dates(Map<String, dynamic> m) {
    final start = field(m, ['starts_on', 'start_date']);
    final end = field(m, ['ends_on', 'end_date']);
    if (start.isEmpty && end.isEmpty) return '';
    if (end.isEmpty || end == start) return formatDate(start);
    return '${formatDate(start)} – ${formatDate(end)}';
  }
}

class _Meta extends StatelessWidget {
  const _Meta(this.icon, this.text);

  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 14, color: scheme.outline),
        const SizedBox(width: 4),
        Text(text, style: AppText.bodySmall),
      ],
    );
  }
}

/// Mission detail — the full record, with the lifecycle visible as a trail.
class MissionDetailScreen extends StatelessWidget {
  const MissionDetailScreen({super.key, required this.mission});

  final Map<String, dynamic> mission;

  static const _lifecycle = ['request', 'review', 'approval', 'execution', 'debrief', 'report'];

  @override
  Widget build(BuildContext context) {
    final current = field(mission, ['stage', 'status'], fallback: 'request').toLowerCase();
    final currentIndex = _lifecycle.indexOf(current);

    return Scaffold(
      appBar: AppBar(title: Text(field(mission, ['title'], fallback: 'Mission'))),
      body: ListView(
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(child: Text('Lifecycle', style: AppText.titleMedium)),
                    StatusBadge(current),
                  ],
                ),
                const SizedBox(height: AppSpacing.md),
                for (var i = 0; i < _lifecycle.length; i++)
                  _StageRow(
                    label: _stageLabel(_lifecycle[i]),
                    done: currentIndex >= 0 && i <= currentIndex,
                    active: i == currentIndex,
                  ),
              ],
            ),
          ),
          const SizedBox(height: AppSpacing.md),
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Details', style: AppText.titleMedium),
                const SizedBox(height: AppSpacing.md),
                for (final entry in <List<String>>[
                  ['Purpose', field(mission, ['purpose', 'description'])],
                  ['Objectives', field(mission, ['objectives'])],
                  ['Success Criteria', field(mission, ['success_criteria'])],
                  ['Expected Impact', field(mission, ['expected_impact'])],
                  ['Category', field(mission, ['category'])],
                  ['Lodge', field(mission, ['lodge_name'])],
                  ['Location', field(mission, ['location'])],
                  ['Starts', formatDate(field(mission, ['starts_on']))],
                  ['Ends', formatDate(field(mission, ['ends_on']))],
                  ['Resources Needed', field(mission, ['resources_needed'])],
                  ['Risk Notes', field(mission, ['risk_notes'])],
                  ['Reviewed By', field(mission, ['reviewed_by'])],
                  ['Approved By', field(mission, ['approved_by'])],
                  ['Guidance', field(mission, ['guidance'])],
                ])
                  if (entry[1].isNotEmpty) DetailField(label: entry[0], value: entry[1]),
              ],
            ),
          ),
        ],
      ),
    );
  }

  static String _stageLabel(String stage) => switch (stage) {
        'request' => 'Request',
        'review' => 'Review',
        'approval' => 'Approval',
        'execution' => 'Execution',
        'debrief' => 'Debrief',
        'report' => 'Report',
        _ => stage,
      };
}

class _StageRow extends StatelessWidget {
  const _StageRow({required this.label, required this.done, required this.active});

  final String label;
  final bool done;
  final bool active;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final colour = done ? scheme.primary : scheme.outline;
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.sm),
      child: Row(
        children: [
          Icon(
            done ? Icons.check_circle : Icons.radio_button_unchecked,
            size: 18,
            color: colour,
          ),
          const SizedBox(width: AppSpacing.sm),
          Text(
            label,
            style: AppText.bodyMedium.copyWith(
              color: done ? scheme.onSurface : scheme.outline,
              fontWeight: active ? FontWeight.w600 : FontWeight.w400,
            ),
          ),
        ],
      ),
    );
  }
}
