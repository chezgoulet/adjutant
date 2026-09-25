import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Members — the roster, searchable, grouped by Lodge when the data says so.
class MembersScreen extends StatefulWidget {
  const MembersScreen({super.key});

  @override
  State<MembersScreen> createState() => _MembersScreenState();
}

class _MembersScreenState extends State<MembersScreen> {
  List<Map<String, dynamic>> _members = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  String _query = '';

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
      final cached = await session.cachedList('members', () => session.api.members());
      if (!mounted) return;
      setState(() {
        _members = cached.value;
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

  List<Map<String, dynamic>> get _visible {
    if (_query.isEmpty) return _members;
    final q = _query.toLowerCase();
    return _members.where((m) {
      final haystack = [
        field(m, ['display_name']),
        field(m, ['username']),
        field(m, ['trail_name']),
        field(m, ['patrol']),
        field(m, ['lodge']),
      ].join(' ').toLowerCase();
      return haystack.contains(q);
    }).toList();
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Padding(
          padding: const EdgeInsets.all(AppSpacing.md),
          child: TextField(
            decoration: const InputDecoration(
              hintText: 'Search members',
              prefixIcon: Icon(Icons.search),
            ),
            onChanged: (v) => setState(() => _query = v.trim()),
          ),
        ),
        Expanded(
          child: _error != null && _members.isEmpty
              ? EmptyState(
                  icon: Icons.cloud_off,
                  title: 'Cannot reach the server',
                  message: _error!,
                  action: FilledButton(onPressed: _load, child: const Text('Retry')),
                )
              : _visible.isEmpty
                  ? const EmptyState(
                      icon: Icons.groups_outlined,
                      title: 'No members found',
                      message: 'Nobody matches that search. The roster comes from the '
                          'membership plugin — if it is empty, the troop has no registered '
                          'members yet.',
                    )
                  : RefreshIndicator(
                      onRefresh: _load,
                      child: ListView.separated(
                        padding: const EdgeInsets.symmetric(horizontal: AppSpacing.md),
                        itemCount: _visible.length,
                        separatorBuilder: (_, _) => const SizedBox(height: AppSpacing.sm),
                        itemBuilder: (context, i) => _MemberCard(_visible[i]),
                      ),
                    ),
        ),
      ],
    );
  }
}

class _MemberCard extends StatelessWidget {
  const _MemberCard(this.member);

  final Map<String, dynamic> member;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final name = field(member, ['display_name', 'username'], fallback: 'Unknown');
    final trail = field(member, ['trail_name']);
    final patrol = field(member, ['patrol']);
    final lodge = field(member, ['lodge']);
    final active = _isActive(member);

    return AppCard(
      child: Row(
        children: [
          CircleAvatar(
            radius: 20,
            backgroundColor: scheme.primaryContainer,
            child: Text(
              name.isEmpty ? '?' : name[0].toUpperCase(),
              style: AppText.titleMedium.copyWith(color: scheme.onPrimaryContainer),
            ),
          ),
          const SizedBox(width: AppSpacing.md),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Flexible(child: Text(name, style: AppText.titleMedium)),
                    if (trail.isNotEmpty) ...[
                      const SizedBox(width: AppSpacing.sm),
                      Text(
                        '“$trail”',
                        style: AppText.bodySmall.copyWith(
                          fontStyle: FontStyle.italic,
                          color: scheme.outline,
                        ),
                      ),
                    ],
                  ],
                ),
                if (patrol.isNotEmpty || lodge.isNotEmpty) ...[
                  const SizedBox(height: 2),
                  Text(
                    [patrol, lodge].where((s) => s.isNotEmpty).join(' · '),
                    style: AppText.bodySmall,
                  ),
                ],
              ],
            ),
          ),
          const SizedBox(width: AppSpacing.sm),
          StatusBadge(active ? 'active' : 'inactive'),
        ],
      ),
    );
  }

  static bool _isActive(Map<String, dynamic> m) {
    final raw = m['is_active'];
    if (raw is bool) return raw;
    return raw?.toString().toLowerCase() != 'false';
  }
}
