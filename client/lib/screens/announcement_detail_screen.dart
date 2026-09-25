import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// One announcement, opened.
///
/// Opening a notice is what records the receipt — that is what a receipt means,
/// and the server's own route is idempotent precisely so this is safe to do on
/// open. A failed receipt never costs the reader the notice: the body is already
/// on screen and the control to write it again is still there.
///
/// The read state is the server's (`is_read`, `my_receipt`, `read_count`), never
/// a local guess, and the count is stated as "read by N" rather than as a
/// percentage because the roster's size is not this plugin's to know.
class AnnouncementDetailScreen extends StatefulWidget {
  const AnnouncementDetailScreen({super.key, required this.id});

  final String id;

  @override
  State<AnnouncementDetailScreen> createState() =>
      _AnnouncementDetailScreenState();
}

class _AnnouncementDetailScreenState extends State<AnnouncementDetailScreen> {
  Map<String, dynamic> _announcement = const {};
  bool _isRead = false;
  int? _readCount;
  DateTime? _readAt;

  /// The server's own sentence about delivery, kept verbatim rather than
  /// paraphrased by this screen.
  String _delivery = '';
  bool _loading = true;
  bool _busy = false;
  String? _error;
  int? _errorStatus;

  /// The receipt is written once, on open. Retrying it is the explicit control's
  /// job, not a loop's.
  bool _receiptAttempted = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load({bool silent = false}) async {
    final session = context.read<SessionState>();
    setState(() {
      if (!silent) _loading = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      final data = await session.api.announcement(widget.id);
      final announcement =
          (data['announcement'] as Map?)?.cast<String, dynamic>() ?? const {};
      if (!mounted) return;
      setState(() {
        _announcement = announcement;
        _isRead = data['is_read'] == true;
        _readCount = (data['read_count'] as num?)?.toInt();
        _readAt = DateTime.tryParse(
          field((data['my_receipt'] as Map?)?.cast<String, dynamic>() ?? const {},
              ['read_at']),
        );
        _delivery = field(data, ['delivery']);
        _loading = false;
      });
      await _maybeRecordReceipt();
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
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

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  String get _status =>
      field(_announcement, ['status'], fallback: 'published').toLowerCase();

  bool get _published => _status == 'published';

  bool get _urgent => field(_announcement, ['category']).toLowerCase() == 'urgent';

  /// Opening an unread notice records the receipt, exactly once, silently.
  Future<void> _maybeRecordReceipt() async {
    if (_receiptAttempted || _isRead || !_published) return;
    _receiptAttempted = true;
    await _writeReceipt(true, quiet: true);
  }

  /// Mark read, or unread. Both are the caller's own receipt — not an
  /// administrative act — so both are offered plainly.
  Future<void> _writeReceipt(bool read, {bool quiet = false}) async {
    final session = context.read<SessionState>();
    if (!quiet) setState(() => _busy = true);
    try {
      final response = read
          ? await session.api.markAnnouncementRead(widget.id)
          : await session.api.markAnnouncementUnread(widget.id);
      // The response carries the fresh badge: the shell's count comes from the
      // server's arithmetic rather than ours.
      session.setAnnouncementBadge(
        (response['unread'] as Map?)?.cast<String, dynamic>(),
      );
      if (!mounted) return;
      setState(() => _isRead = read);
      // `read_count` moves with a receipt and only the server knows it, so ask
      // again rather than adjusting a number locally.
      await _load(silent: true);
      if (!mounted || quiet) return;
      _say(read ? 'Marked read' : 'Marked unread');
    } on ApiException catch (e) {
      if (!mounted) return;
      // A refusal (a draft has no readers to record) is the server explaining
      // itself, so its words are the message.
      if (!quiet) _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      if (!quiet) {
        _say(
          'Cannot reach the server — the receipt was not written. Nothing else '
          'has changed.',
          bad: true,
        );
      }
      debugPrint('receipt write failed: $e');
    } finally {
      if (mounted && !quiet) setState(() => _busy = false);
    }
  }

  void _say(String message, {bool bad = false}) {
    final scheme = Theme.of(context).colorScheme;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text(message),
        backgroundColor: bad ? scheme.errorContainer : null,
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Announcement'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _loading ? null : _load,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : _refused
              ? EmptyState(
                  icon: Icons.lock_outline,
                  title: 'Not sent to a scope you hold',
                  // The server answers 403 for a notice addressed elsewhere and
                  // for one that does not exist — deliberately the same answer,
                  // so this screen repeats it rather than guessing which.
                  message: _error ??
                      'This announcement is not addressed to any scope you hold.',
                )
              : _announcement.isEmpty
                  ? EmptyState(
                      icon: Icons.cloud_off,
                      title: 'Cannot reach the server',
                      message: _error ?? 'Nothing came back for this announcement.',
                      action: FilledButton(
                        onPressed: _load,
                        child: const Text('Retry'),
                      ),
                    )
                  : _body(),
    );
  }

  Widget _body() {
    final scheme = Theme.of(context).colorScheme;
    final title = field(_announcement, ['title'], fallback: 'Untitled notice');
    final body = field(_announcement, ['body']);
    final published = formatDate(
      field(_announcement, ['published_at', 'created_at']),
      withTime: true,
    );
    final expires = field(_announcement, ['expires_at']);
    final scopeType = field(_announcement, ['scope_type']).toLowerCase();
    final scopeId = field(_announcement, ['scope_id']);
    final scope = scopeType == 'lodge'
        ? (scopeId.isEmpty ? 'Lodge' : 'Lodge $scopeId')
        : scopeType == 'troop'
            ? 'Troop-wide'
            : scopeType;
    final category = field(_announcement, ['category'], fallback: 'informational');

    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        if (_urgent)
          Container(
            margin: const EdgeInsets.only(bottom: AppSpacing.md),
            padding: const EdgeInsets.all(AppSpacing.sm),
            decoration: BoxDecoration(
              color: AppColors.error,
              borderRadius: BorderRadius.circular(AppRadius.sm),
            ),
            child: Row(
              children: [
                const Icon(Icons.priority_high, size: 20, color: Colors.white),
                const SizedBox(width: AppSpacing.sm),
                Expanded(
                  child: Text(
                    'Urgent — this notice is meant to interrupt',
                    style: AppText.labelLarge.copyWith(color: Colors.white),
                  ),
                ),
              ],
            ),
          ),
        Text(title, style: AppText.headlineMedium),
        const SizedBox(height: AppSpacing.sm),
        Wrap(
          spacing: AppSpacing.sm,
          runSpacing: AppSpacing.xs,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            StatusBadge(_status, label: _published ? null : _titleCase(_status)),
            Text(
              [category, scope, published]
                  .where((s) => s.isNotEmpty && s != '—')
                  .join(' · '),
              style: AppText.bodySmall,
            ),
          ],
        ),
        if (_status == 'retracted') ...[
          const SizedBox(height: AppSpacing.sm),
          Text(
            'Retracted by its sender. It stays here for the record and no longer '
            'counts toward the badge.',
            style: AppText.bodySmall.copyWith(color: scheme.error),
          ),
        ],
        const SizedBox(height: AppSpacing.md),
        const Divider(),
        const SizedBox(height: AppSpacing.md),
        if (body.isEmpty)
          Text(
            'This notice has no further detail — the title is the whole message.',
            style: AppText.bodyMedium.copyWith(color: scheme.outline),
          )
        else
          ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 640),
            child: Text(body, style: AppText.bodyLarge),
          ),
        if (expires.isNotEmpty) ...[
          const SizedBox(height: AppSpacing.md),
          Text(
            'Expires ${formatDate(expires, withTime: true)}',
            style: AppText.bodySmall,
          ),
        ],
        const SizedBox(height: AppSpacing.lg),
        const Divider(),
        const SizedBox(height: AppSpacing.md),
        _receipt(scheme),
        const SizedBox(height: AppSpacing.lg),
        Text(
          _delivery.isEmpty
              ? 'Delivery is deferred: this inbox is where the notice lands. No '
                  'push notification is sent yet.'
              : 'Delivery: $_delivery',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
      ],
    );
  }

  Widget _receipt(ColorScheme scheme) {
    final read = _isRead;
    final count = _readCount;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Icon(
              read ? Icons.check_circle_outline : Icons.circle,
              size: 16,
              color: read ? scheme.primary : scheme.outline,
            ),
            const SizedBox(width: AppSpacing.sm),
            Expanded(
              child: Text(
                read
                    ? 'You have read this'
                        '${_readAt == null ? '' : ' (${formatDate(_readAt!.toIso8601String(), withTime: true)})'}'
                    : 'You have not read this yet',
                style: AppText.titleMedium,
              ),
            ),
          ],
        ),
        if (count != null) ...[
          const SizedBox(height: AppSpacing.xs),
          // Who read, never who has not — that answer needs the roster, which a
          // plugin cannot see, so this is the honest form of the fact.
          Text('Read by $count', style: AppText.bodySmall),
        ],
        const SizedBox(height: AppSpacing.md),
        if (_busy)
          const SizedBox(
            width: 24,
            height: 24,
            child: CircularProgressIndicator(strokeWidth: 2),
          )
        else if (_published)
          FilledButton.icon(
            onPressed: () => _writeReceipt(!read),
            icon: Icon(
              read ? Icons.mark_email_unread_outlined : Icons.done,
              size: 20,
            ),
            label: Text(read ? 'Mark unread again' : 'Mark as read'),
          )
        else
          // A draft has no readers to record: the server refuses, and the
          // control that would earn that refusal is simply not offered here.
          Text(
            'Read receipts are recorded for published announcements only.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
      ],
    );
  }

  static String _titleCase(String raw) =>
      raw.isEmpty ? raw : raw.substring(0, 1).toUpperCase() + raw.substring(1);
}
