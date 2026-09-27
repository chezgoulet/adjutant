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
      // Scrollable, so a long message is never clipped: a short window (a
      // landscape phone, a small desktop window) must show the whole
      // explanation rather than the top half of it, and this is the widget that
      // has to keep the "never a blank screen" promise.
      child: SingleChildScrollView(
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

/// The one place a failed read becomes words — and the one place the decision
/// about Retry is made.
///
/// A failure has exactly two shapes, and folding them together is the bug this
/// exists to prevent:
///
///  * **The server answered** ([ApiException]): it holds a status *and its own
///    sentence about what went wrong*. Rendering that as "Cannot reach the
///    server" is a claim the reader cannot act on, and Retry cannot change an
///    answer already given. A 404 is most often a route this deployment does
///    not run — a plugin that is switched off answers exactly that — so it is
///    named as not-found, the server's own sentence is kept verbatim beneath it,
///    and no Retry is offered. Every other 4xx is the server refusing this
///    request for a reason only it knows, so its sentence is the message and
///    again there is nothing to retry. A 5xx is the server failing to answer
///    something it does run, which may well work on a second attempt, so Retry
///    stays.
///  * **Nothing answered** (no status: [OfflineException], or anything
///    unrecognised): the transport failed and the same request may well succeed
///    next time. This is the only case that reads "Cannot reach the server", and
///    the only case where Retry is honest.
class FailureView {
  const FailureView({
    required this.icon,
    required this.title,
    required this.message,
    required this.retryable,
  });

  final IconData icon;
  final String title;
  final String message;

  /// Whether offering Retry is honest — see the class doc.
  final bool retryable;
}

/// Classify a failed read: the status and sentence the screen already holds in,
/// the words to render and whether Retry means anything out.
///
/// [error] is the message the screen already holds — `ApiException.message`,
/// i.e. the server's own words, or a transport failure's text — and
/// [statusCode] is `null` when no HTTP answer arrived at all. [missingTitle]
/// lets a screen that reads one named record say what is missing ("No such
/// item", "No such motion"); a collection keeps the honest, non-committal
/// default rather than inventing a reason for the absence.
FailureView describeFailure(
  String? error,
  int? statusCode, {
  String missingTitle = 'Not found on this server',
}) {
  final said = (error ?? '').trim();
  if (statusCode == null) {
    return FailureView(
      icon: Icons.cloud_off,
      title: 'Cannot reach the server',
      message: said.isEmpty ? 'No answer came back from the server.' : said,
      retryable: true,
    );
  }
  if (statusCode == 404) {
    return FailureView(
      icon: Icons.search_off,
      title: missingTitle,
      message: said.isEmpty ? 'The server has nothing at this route.' : said,
      retryable: false,
    );
  }
  if (statusCode < 500) {
    return FailureView(
      icon: Icons.report_problem_outlined,
      title: 'The server refused this request',
      message: said.isEmpty ? 'The server refused this request.' : said,
      retryable: false,
    );
  }
  return FailureView(
    icon: Icons.cloud_off,
    title: 'The server could not answer',
    message: said.isEmpty ? 'The server answered with $statusCode.' : said,
    retryable: true,
  );
}

/// The empty state for a failed read: [describeFailure]'s words, with Retry
/// offered only where it can change the answer.
///
/// Screens with a refusal of their own to state (401/403, named in words with
/// the permission that would fix it) branch before this; everything else that
/// folds a failure into "Cannot reach the server" uses this one rule.
EmptyState failureState({
  required String? error,
  required int? statusCode,
  VoidCallback? onRetry,
  String missingTitle = 'Not found on this server',
}) {
  final view = describeFailure(error, statusCode, missingTitle: missingTitle);
  return EmptyState(
    icon: view.icon,
    title: view.title,
    message: view.message,
    action: view.retryable && onRetry != null
        ? FilledButton(onPressed: onRetry, child: const Text('Retry'))
        : null,
  );
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

/// Money, as the finance plugin states it: an integer number of cents, rendered
/// here the way the server renders it (`$1,234.56`, two decimals, grouped).
///
/// A balance is derived server-side from the ledger and never stored, so this
/// only ever *renders* a figure the server computed — the client does not add
/// up money, because a second implementation of that is a second answer.
String formatCents(int? cents) {
  if (cents == null) return '—';
  final abs = cents.abs();
  final digits = (abs ~/ 100).toString();
  final remainder = (abs % 100).toString().padLeft(2, '0');
  final grouped = StringBuffer();
  for (var i = 0; i < digits.length; i++) {
    if (i > 0 && (digits.length - i) % 3 == 0) grouped.write(',');
    grouped.write(digits[i]);
  }
  return '${cents < 0 ? '-' : ''}\$$grouped.$remainder';
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
