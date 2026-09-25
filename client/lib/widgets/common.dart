/// The shared widget vocabulary. A plugin's data selects these; it never
/// supplies a colour, a size, or a style of its own.
library;

import 'package:flutter/material.dart';

import '../theme/app_theme.dart';

/// A status as a badge. Colour is never the only signal — the label carries the
/// meaning, so the badge survives a greyscale screen and a colour-blind reader
/// (design language §10).
class StatusBadge extends StatelessWidget {
  const StatusBadge(this.status, {super.key, this.label});

  final String status;
  final String? label;

  /// Human wording for the stage/state strings the API returns.
  static const _labels = {
    'request': 'Requested',
    'review': 'In Review',
    'approval': 'Awaiting Approval',
    'approved': 'Approved',
    'execution': 'In Progress',
    'in_progress': 'In Progress',
    'debrief': 'Debrief',
    'report': 'Report',
    'reported': 'Reported',
    'completed': 'Completed',
    'rejected': 'Rejected',
    'withdrawn': 'Withdrawn',
    'active': 'Active',
    'inactive': 'Inactive',
    'scheduled': 'Scheduled',
    'cancelled': 'Cancelled',
    'proposed': 'Proposed',
    'seconded': 'Seconded',
    'debate': 'In Debate',
    'voting': 'Voting',
    'decided': 'Decided',
    'passed': 'Passed',
    'failed': 'Failed',
    'implemented': 'Implemented',
    'open': 'Open',
  };

  /// Map a stage/state onto one of the semantic colour families.
  static String _family(String status) {
    switch (status.toLowerCase()) {
      case 'approved':
      case 'completed':
      case 'reported':
      case 'passed':
      case 'implemented':
      case 'active':
        return 'approved';
      case 'rejected':
      case 'failed':
      case 'cancelled':
      case 'withdrawn':
        return 'rejected';
      case 'review':
      case 'approval':
      case 'voting':
      case 'proposed':
      case 'seconded':
        return 'pending';
      case 'execution':
      case 'in_progress':
      case 'debrief':
        return 'in_progress';
      default:
        return 'neutral';
    }
  }

  @override
  Widget build(BuildContext context) {
    final brightness = Theme.of(context).brightness;
    final family = _family(status);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        color: AppColors.statusContainer(family, brightness),
        borderRadius: BorderRadius.circular(20),
      ),
      child: Text(
        label ?? _labels[status.toLowerCase()] ?? status,
        style: AppText.labelMedium.copyWith(
          color: AppColors.statusForeground(family, brightness),
        ),
      ),
    );
  }
}

/// A card. The primary container for content everywhere in the app.
class AppCard extends StatelessWidget {
  const AppCard({super.key, required this.child, this.onTap, this.padding});

  final Widget child;
  final VoidCallback? onTap;
  final EdgeInsetsGeometry? padding;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final content = Padding(
      padding: padding ?? const EdgeInsets.all(AppSpacing.md),
      child: child,
    );
    return Material(
      color: scheme.surface,
      borderRadius: BorderRadius.circular(AppRadius.md),
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(AppRadius.md),
        child: Container(
          decoration: BoxDecoration(
            borderRadius: BorderRadius.circular(AppRadius.md),
            border: Border.all(color: scheme.outlineVariant),
          ),
          child: content,
        ),
      ),
    );
  }
}

/// Empty states are how a new scout learns the app. Never a blank screen.
class EmptyState extends StatelessWidget {
  const EmptyState({
    super.key,
    required this.icon,
    required this.title,
    required this.message,
    this.action,
  });

  final IconData icon;
  final String title;
  final String message;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(AppSpacing.xl),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 48, color: scheme.outline),
            const SizedBox(height: AppSpacing.md),
            Text(title, style: AppText.titleLarge, textAlign: TextAlign.center),
            const SizedBox(height: AppSpacing.sm),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 320),
              child: Text(
                message,
                style: AppText.bodyMedium.copyWith(color: scheme.onSurface.withValues(alpha: 0.6)),
                textAlign: TextAlign.center,
              ),
            ),
            if (action != null) ...[
              const SizedBox(height: AppSpacing.md),
              action!,
            ],
          ],
        ),
      ),
    );
  }
}

/// The offline notice. A normal state, stated plainly — not an error dialog.
class OfflineBanner extends StatelessWidget {
  const OfflineBanner({super.key, this.cachedAt});

  final DateTime? cachedAt;

  @override
  Widget build(BuildContext context) {
    final when = cachedAt;
    final detail = when == null
        ? 'changes will sync when connected'
        : 'showing data from ${when.hour.toString().padLeft(2, '0')}:'
            '${when.minute.toString().padLeft(2, '0')}';
    return Container(
      width: double.infinity,
      color: AppColors.warning,
      padding: const EdgeInsets.symmetric(horizontal: AppSpacing.md, vertical: 6),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          const Icon(Icons.cloud_off, size: 16, color: Colors.black87),
          const SizedBox(width: AppSpacing.sm),
          Flexible(
            child: Text(
              'Offline — $detail',
              style: AppText.labelMedium.copyWith(color: Colors.black87),
            ),
          ),
        ],
      ),
    );
  }
}

/// A labelled value pair, for detail screens. Keeps detail layouts honest:
/// label above, value below, no invented decoration.
class DetailField extends StatelessWidget {
  const DetailField({super.key, required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.md),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            label.toUpperCase(),
            style: AppText.labelSmall.copyWith(
              color: scheme.onSurface.withValues(alpha: 0.55),
              letterSpacing: 0.5,
            ),
          ),
          const SizedBox(height: 2),
          Text(value.isEmpty ? '—' : value, style: AppText.bodyLarge),
        ],
      ),
    );
  }
}

/// Formats a value from an API map without ceremony. Every field is absent
/// until the server says otherwise; this makes that safe and readable.
String field(Map<String, dynamic> map, List<String> keys, {String fallback = ''}) {
  for (final key in keys) {
    final value = map[key];
    if (value == null) continue;
    if (value is String && value.trim().isEmpty) continue;
    if (value is List) return value.join(', ');
    return value.toString();
  }
  return fallback;
}

/// ISO-8601 from the server, rendered in the one format the design language
/// allows for field use: 24-hour, unambiguous.
String formatDate(String? iso, {bool withTime = false}) {
  if (iso == null || iso.isEmpty) return '—';
  final parsed = DateTime.tryParse(iso);
  if (parsed == null) return iso;
  final local = parsed.toLocal();
  final d = '${local.day.toString().padLeft(2, '0')}/'
      '${local.month.toString().padLeft(2, '0')}/${local.year}';
  if (!withTime) return d;
  return '$d ${local.hour.toString().padLeft(2, '0')}:'
      '${local.minute.toString().padLeft(2, '0')}';
}

/// "Today", "Tomorrow", or a date — for upcoming lists where the relative day
/// is the useful fact.
String formatRelativeDate(String? iso) {
  if (iso == null || iso.isEmpty) return '—';
  final parsed = DateTime.tryParse(iso);
  if (parsed == null) return iso;
  final local = parsed.toLocal();
  final today = DateTime.now();
  final days = DateTime(local.year, local.month, local.day)
      .difference(DateTime(today.year, today.month, today.day))
      .inDays;
  if (days == 0) return 'Today';
  if (days == 1) return 'Tomorrow';
  if (days == -1) return 'Yesterday';
  return formatDate(iso);
}
