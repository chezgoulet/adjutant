import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Calendar — where a troop actually plans its life, and where Congress quorum
/// becomes visible before the meeting rather than at it.
class CalendarScreen extends StatefulWidget {
  const CalendarScreen({super.key});

  @override
  State<CalendarScreen> createState() => _CalendarScreenState();
}

class _CalendarScreenState extends State<CalendarScreen> {
  List<Map<String, dynamic>> _events = const [];
  List<Map<String, dynamic>> _upcoming = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  DateTime _month = DateTime.now();

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
      final events = await session.cachedList('events', () => session.api.events());
      final upcoming = await session.cachedList('upcoming', session.api.upcomingEvents);
      if (!mounted) return;
      setState(() {
        _events = events.value;
        _upcoming = upcoming.value;
        _stale = events.isStale;
        _cachedAt = events.cachedAt;
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

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Expanded(
          child: _error != null && _events.isEmpty && _upcoming.isEmpty
              ? EmptyState(
                  icon: Icons.cloud_off,
                  title: 'Cannot reach the server',
                  message: _error!,
                  action: FilledButton(onPressed: _load, child: const Text('Retry')),
                )
              : RefreshIndicator(
                  onRefresh: _load,
                  child: ListView(
                    padding: const EdgeInsets.all(AppSpacing.md),
                    children: [
                      _MonthGrid(
                        month: _month,
                        events: _events,
                        onPrev: () => setState(
                            () => _month = DateTime(_month.year, _month.month - 1)),
                        onNext: () => setState(
                            () => _month = DateTime(_month.year, _month.month + 1)),
                      ),
                      const SizedBox(height: AppSpacing.md),
                      _UpcomingList(events: _upcoming, all: _events),
                    ],
                  ),
                ),
        ),
      ],
    );
  }
}

class _MonthGrid extends StatelessWidget {
  const _MonthGrid({
    required this.month,
    required this.events,
    required this.onPrev,
    required this.onNext,
  });

  final DateTime month;
  final List<Map<String, dynamic>> events;
  final VoidCallback onPrev;
  final VoidCallback onNext;

  static const _monthNames = [
    'January', 'February', 'March', 'April', 'May', 'June',
    'July', 'August', 'September', 'October', 'November', 'December',
  ];

  /// Dates in this month that carry an event.
  Set<int> _eventDays() {
    final days = <int>{};
    for (final e in events) {
      final raw = field(e, ['starts_at', 'start', 'starts_on']);
      final parsed = DateTime.tryParse(raw)?.toLocal();
      if (parsed != null && parsed.year == month.year && parsed.month == month.month) {
        days.add(parsed.day);
      }
    }
    return days;
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final first = DateTime(month.year, month.month, 1);
    final daysInMonth = DateTime(month.year, month.month + 1, 0).day;
    // Sunday-first grid, matching the design language mockup.
    final leading = first.weekday % 7;
    final today = DateTime.now();
    final eventDays = _eventDays();

    return AppCard(
      child: Column(
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  '${_monthNames[month.month - 1]} ${month.year}',
                  style: AppText.titleLarge,
                ),
              ),
              IconButton(
                onPressed: onPrev,
                icon: const Icon(Icons.chevron_left),
                tooltip: 'Previous month',
              ),
              IconButton(
                onPressed: onNext,
                icon: const Icon(Icons.chevron_right),
                tooltip: 'Next month',
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          GridView.count(
            crossAxisCount: 7,
            shrinkWrap: true,
            physics: const NeverScrollableScrollPhysics(),
            childAspectRatio: 1.1,
            children: [
              for (final d in ['S', 'M', 'T', 'W', 'T', 'F', 'S'])
                Center(
                  child: Text(
                    d,
                    style: AppText.labelMedium.copyWith(color: scheme.outline),
                  ),
                ),
              for (var i = 0; i < leading; i++) const SizedBox.shrink(),
              for (var day = 1; day <= daysInMonth; day++)
                _DayCell(
                  day: day,
                  today: today.year == month.year &&
                      today.month == month.month &&
                      today.day == day,
                  hasEvent: eventDays.contains(day),
                ),
            ],
          ),
        ],
      ),
    );
  }
}

class _DayCell extends StatelessWidget {
  const _DayCell({required this.day, required this.today, required this.hasEvent});

  final int day;
  final bool today;
  final bool hasEvent;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Center(
      child: Container(
        width: 34,
        height: 34,
        decoration: BoxDecoration(
          color: today ? scheme.primary : null,
          shape: BoxShape.circle,
        ),
        child: Stack(
          alignment: Alignment.center,
          children: [
            Text(
              '$day',
              style: AppText.bodyMedium.copyWith(
                color: today ? scheme.onPrimary : scheme.onSurface,
                fontWeight: today ? FontWeight.w600 : FontWeight.w400,
              ),
            ),
            if (hasEvent)
              Positioned(
                bottom: 4,
                child: Container(
                  width: 4,
                  height: 4,
                  decoration: BoxDecoration(
                    color: today ? scheme.onPrimary : scheme.primary,
                    shape: BoxShape.circle,
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

class _UpcomingList extends StatelessWidget {
  const _UpcomingList({required this.events, required this.all});

  final List<Map<String, dynamic>> events;
  final List<Map<String, dynamic>> all;

  /// Quorum, when the event carries it — Congress and meetings show whether the
  /// troop is on track *before* the day, which is the whole point of tracking it.
  Widget? _quorum(BuildContext context, Map<String, dynamic> e) {
    final expected = int.tryParse(field(e, ['expected_voters']));
    final basis = field(e, ['quorum_basis']);
    final going = int.tryParse(field(e, ['going_count', 'rsvp_going']));
    if (expected == null || expected <= 0 || basis.isEmpty) return null;
    final needed = basis == 'one_third_registered'
        ? (expected / 3).ceil()
        : basis == 'majority_members'
            ? (expected ~/ 2) + 1
            : null;
    if (needed == null) return null;
    final have = going ?? 0;
    return Padding(
      padding: const EdgeInsets.only(top: 4),
      child: Text(
        'Quorum: $have/$needed${have >= needed ? ' — met' : ''}',
        style: AppText.bodySmall.copyWith(
          color: have >= needed
              ? AppColors.success
              : Theme.of(context).colorScheme.outline,
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final list = events.isNotEmpty ? events : all;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Upcoming Events', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.md),
          if (list.isEmpty)
            const EmptyState(
              icon: Icons.event_available_outlined,
              title: 'Nothing scheduled',
              message: 'Meetings, campouts, work days, and Congress will appear here once '
                  'they are on the calendar.',
            )
          else
            for (final e in list.take(10))
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.md),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Container(
                      width: 4,
                      height: 52,
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
                              formatRelativeDate(field(e, ['starts_at', 'start', 'starts_on'])),
                              formatDate(field(e, ['starts_at', 'start']), withTime: true)
                                  .split(' ')
                                  .last,
                              field(e, ['location']),
                            ].where((s) => s.isNotEmpty && s != '—').join(' · '),
                            style: AppText.bodySmall,
                          ),
                          _quorum(context, e) ?? const SizedBox.shrink(),
                        ],
                      ),
                    ),
                    const SizedBox(width: AppSpacing.sm),
                    StatusBadge(field(e, ['status', 'event_type'], fallback: 'scheduled')),
                  ],
                ),
              ),
        ],
      ),
    );
  }
}
