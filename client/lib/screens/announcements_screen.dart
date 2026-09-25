import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'announcement_detail_screen.dart';

/// Announcements — the inbox.
///
/// This is daily work, so it lives in the shell's navigation beside Missions and
/// the Calendar rather than behind Settings: the first thing a scout needs on a
/// Friday is the notice that the meeting moved, and burying that two taps deep
/// is how a notice goes unread.
///
/// Urgent is the category that exists to interrupt, so it is rendered as an
/// interruption and not as a filter chip: a red-edge card that names itself,
/// with an icon and the word — legible in greyscale, in daylight, and at night
/// (design language §10, "colour is never the only indicator").
///
/// Offline-first, like every list here: the read goes through
/// [SessionState.cachedList], the last-known inbox is served when the network is
/// gone, and [OfflineBanner] says which hour that truth is from.
class AnnouncementsScreen extends StatefulWidget {
  const AnnouncementsScreen({super.key});

  @override
  State<AnnouncementsScreen> createState() => _AnnouncementsScreenState();
}

class _AnnouncementsScreenState extends State<AnnouncementsScreen> {
  List<Map<String, dynamic>> _items = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;

  /// Unread rows only. The list the server returns already carries each row's
  /// `is_read`, so narrowing to the unread ones here is reading the server's
  /// answer rather than guessing at it — and it keeps working from the cache,
  /// where a second filtered request could not.
  bool _unreadOnly = false;

  /// The announcement currently being marked, so only its own control spins.
  String? _busy;

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
      final cached =
          await session.cachedList('announcements', session.api.announcements);
      if (!mounted) return;
      setState(() {
        _items = cached.value;
        _stale = cached.isStale;
        _cachedAt = cached.cachedAt;
        _loading = false;
      });
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
    // The badge is the caller's whole count, not this page's, and it is a
    // separate read precisely so a filter cannot change what it says. A refusal
    // here is already handled inside the session; the list state above is the
    // one that gets told about.
    if (mounted) await session.refreshAnnouncementBadge();
  }

  /// A refusal is not a network failure: there is nothing to retry, and nothing
  /// is broken — this caller simply holds no scope a notice was sent to.
  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  bool _isUnread(Map<String, dynamic> a) => a['is_read'] != true;

  List<Map<String, dynamic>> get _visible =>
      _unreadOnly ? _items.where(_isUnread).toList() : _items;

  /// Mark read (or unread) from the inbox without opening the notice.
  ///
  /// The server answers with the fresh badge, so the shell's count comes from
  /// its own arithmetic, not ours.
  Future<void> _setRead(Map<String, dynamic> announcement, bool read) async {
    final session = context.read<SessionState>();
    final id = field(announcement, ['id']);
    if (id.isEmpty) return;
    setState(() => _busy = id);
    try {
      final response = read
          ? await session.api.markAnnouncementRead(id)
          : await session.api.markAnnouncementUnread(id);
      session.setAnnouncementBadge(
        (response['unread'] as Map?)?.cast<String, dynamic>(),
      );
      if (!mounted) return;
      await _load(silent: true);
    } on ApiException catch (e) {
      if (!mounted) return;
      _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      _say(
        'Could not reach the server — the announcement is still '
        '${read ? 'unread' : 'read'} on the record.',
        bad: true,
      );
      debugPrint('announcement receipt failed: $e');
    } finally {
      if (mounted) setState(() => _busy = null);
    }
  }

  Future<void> _open(Map<String, dynamic> announcement) async {
    final id = field(announcement, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => AnnouncementDetailScreen(id: id),
      ),
    );
    // Opening one is what records a receipt, so the list and the badge are both
    // out of date on the way back — refreshed silently, so the inbox does not
    // flash a spinner at somebody who just read a notice.
    if (mounted) await _load(silent: true);
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
    if (_loading) return const Center(child: CircularProgressIndicator());

    final badge = context.watch<SessionState>().announcementBadge;
    final unread = (badge?['unread'] as num?)?.toInt();
    final urgent = badge?['has_urgent'] == true;

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        if (!_refused && _error == null) _header(unread, urgent),
        Expanded(
          child: _refused
              ? EmptyState(
                  icon: Icons.lock_outline,
                  title: 'Announcements are not addressed to you',
                  // State the permission, then keep the server's own words: a
                  // bare "forbidden" gives the reader nothing to act on, and the
                  // roles that hold a permission live in a table this client
                  // cannot read.
                  message: 'Reading the inbox needs announcements:read in a scope '
                      'a notice was sent to.'
                      '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
                )
              : _error != null
                  ? EmptyState(
                      icon: Icons.cloud_off,
                      title: 'Cannot reach the server',
                      message: _error!,
                      action: FilledButton(
                        onPressed: _load,
                        child: const Text('Retry'),
                      ),
                    )
                  : _visible.isEmpty
                      ? _emptyInbox()
                      : RefreshIndicator(
                          onRefresh: _load,
                          child: ListView.separated(
                            padding: const EdgeInsets.all(AppSpacing.md),
                            itemCount: _visible.length + 1,
                            separatorBuilder: (_, _) =>
                                const SizedBox(height: AppSpacing.sm),
                            itemBuilder: (context, i) {
                              if (i == _visible.length) return _deliveryNote();
                              final a = _visible[i];
                              return _AnnouncementCard(
                                announcement: a,
                                busy: _busy == field(a, ['id']),
                                onTap: () => _open(a),
                                onToggleRead: () =>
                                    _setRead(a, _isUnread(a)),
                              );
                            },
                          ),
                        ),
        ),
      ],
    );
  }

  Widget _header(int? unread, bool urgent) {
    final scheme = Theme.of(context).colorScheme;
    final visible = _items.length;
    final summary = unread == null
        ? 'Troop announcements'
        : '$unread unread of $visible in this inbox';
    return Padding(
      padding: const EdgeInsets.fromLTRB(
        AppSpacing.md,
        AppSpacing.md,
        AppSpacing.md,
        0,
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(summary, style: AppText.titleMedium),
              ),
              if (urgent)
                Container(
                  padding: const EdgeInsets.symmetric(
                    horizontal: AppSpacing.sm,
                    vertical: AppSpacing.xs,
                  ),
                  decoration: BoxDecoration(
                    color: AppColors.error,
                    borderRadius: BorderRadius.circular(AppRadius.sm),
                  ),
                  child: Text(
                    'Urgent unread',
                    style: AppText.labelMedium.copyWith(color: Colors.white),
                  ),
                ),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Row(
            children: [
              Expanded(
                child: Text(
                  'Unread only',
                  style: AppText.bodyMedium.copyWith(color: scheme.onSurface),
                ),
              ),
              Switch(
                value: _unreadOnly,
                onChanged: (value) => setState(() => _unreadOnly = value),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _emptyInbox() {
    if (_unreadOnly) {
      return EmptyState(
        icon: Icons.mark_email_read_outlined,
        title: 'Nothing unread',
        message: 'Every announcement in this inbox has been read. Switch "Unread '
            'only" off to see them again.',
        action: FilledButton.tonal(
          onPressed: () => setState(() => _unreadOnly = false),
          child: const Text('Show all'),
        ),
      );
    }
    return const EmptyState(
      icon: Icons.inbox_outlined,
      title: 'No announcements',
      message: 'Notices sent to the scopes you hold — troop-wide, or your Lodge — '
          'arrive here. Nothing has been sent to you yet.',
    );
  }

  /// One honest sentence about delivery, once, rather than a promise the system
  /// does not keep: no push provider is wired anywhere in Adjutant yet, so this
  /// inbox is where a notice actually lands.
  Widget _deliveryNote() => Padding(
        padding: const EdgeInsets.only(top: AppSpacing.sm, bottom: AppSpacing.lg),
        child: Text(
          'Delivery is deferred: this inbox is where a notice lands. No push '
          'notification is sent yet.',
          style: AppText.bodySmall.copyWith(
            color: Theme.of(context).colorScheme.outline,
          ),
        ),
      );
}

/// One notice as a row: urgent ones wear the red edge and name themselves.
class _AnnouncementCard extends StatelessWidget {
  const _AnnouncementCard({
    required this.announcement,
    required this.busy,
    required this.onTap,
    required this.onToggleRead,
  });

  final Map<String, dynamic> announcement;
  final bool busy;
  final VoidCallback onTap;
  final VoidCallback onToggleRead;

  /// Absent `is_read` counts as unread: a notice whose receipt state the server
  /// did not state is shown as needing attention, never hidden as handled.
  bool get _unread => announcement['is_read'] != true;

  String get _category {
    final raw = field(announcement, ['category'], fallback: 'informational');
    return raw.substring(0, 1).toUpperCase() + raw.substring(1);
  }

  bool get _urgent =>
      field(announcement, ['category']).toLowerCase() == 'urgent';

  String get _scope {
    final type = field(announcement, ['scope_type']).toLowerCase();
    final id = field(announcement, ['scope_id']);
    if (type == 'lodge') return id.isEmpty ? 'Lodge' : 'Lodge $id';
    if (type == 'troop') return 'Troop-wide';
    return type.isEmpty ? '' : type;
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final title = field(announcement, ['title'], fallback: 'Untitled notice');
    final published = formatRelativeDate(
      field(announcement, ['published_at', 'created_at']),
    );
    final meta = [published, _scope].where((s) => s.isNotEmpty && s != '—').join(' · ');

    final content = AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          if (_urgent) ...[
            Container(
              padding: const EdgeInsets.symmetric(
                horizontal: AppSpacing.sm,
                vertical: AppSpacing.xs,
              ),
              decoration: BoxDecoration(
                color: AppColors.error,
                borderRadius: BorderRadius.circular(AppRadius.sm),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Icon(Icons.priority_high, size: 16, color: Colors.white),
                  const SizedBox(width: AppSpacing.xs),
                  Text(
                    'Urgent',
                    style: AppText.labelMedium.copyWith(color: Colors.white),
                  ),
                ],
              ),
            ),
            const SizedBox(height: AppSpacing.sm),
          ],
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              // An unread dot and a read one, legible without colour: the shape
              // says which, and the action label beside it says it again.
              Padding(
                padding: const EdgeInsets.only(top: 6, right: AppSpacing.sm),
                child: Icon(
                  _unread ? Icons.circle : Icons.check_circle_outline,
                  size: 12,
                  color: _unread ? scheme.primary : scheme.outline,
                ),
              ),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      title,
                      style: (_unread ? AppText.titleLarge : AppText.titleMedium)
                          .copyWith(
                        fontWeight: _unread ? FontWeight.w700 : FontWeight.w500,
                      ),
                    ),
                    if (meta.isNotEmpty) ...[
                      const SizedBox(height: 2),
                      Text(meta, style: AppText.bodySmall),
                    ],
                  ],
                ),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          Row(
            children: [
              if (!_urgent) StatusBadge(field(announcement, ['category']), label: _category),
              const Spacer(),
              if (busy)
                const SizedBox(
                  width: 20,
                  height: 20,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              else
                TextButton.icon(
                  onPressed: onToggleRead,
                  icon: Icon(
                    _unread ? Icons.done : Icons.mark_email_unread_outlined,
                    size: 18,
                  ),
                  label: Text(_unread ? 'Mark read' : 'Mark unread'),
                ),
            ],
          ),
        ],
      ),
    );

    if (!_urgent) return content;
    // The red edge is the second signal, not the only one: the strip inside says
    // "Urgent" in words for a greyscale screen and a colour-blind reader.
    return Container(
      decoration: BoxDecoration(
        border: Border.all(color: AppColors.error, width: 2),
        borderRadius: BorderRadius.circular(AppRadius.md),
      ),
      child: content,
    );
  }
}
