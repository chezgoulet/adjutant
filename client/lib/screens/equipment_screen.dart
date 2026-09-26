import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'equipment_item_screen.dart';

/// Equipment — the gear pool, the scout's side of it (SPEC §7.6).
///
/// Three questions a scout actually asks, one per tab:
///
///  * **Catalogue** — what does the troop own? (`GET /api/equipment/items`)
///  * **Available** — what can I take on these dates, and if not, why not?
///    (`GET /api/equipment/availability`)
///  * **I have out** — what am I holding, and when is it due?
///    (`GET /api/equipment/checkouts?state=open&member=<me>`)
///
/// Nothing here decides eligibility. The pool, the refusals and the promises
/// are the server's answers, and a refusal — retired, in maintenance,
/// unserviceable, already out — is rendered in the server's own words rather
/// than hidden behind a generic failure. Checking gear out and back in lives
/// on the item, reached by tapping a row.
class EquipmentScreen extends StatelessWidget {
  const EquipmentScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return DefaultTabController(
      length: 3,
      child: Column(
        children: [
          const TabBar(
            tabs: [
              Tab(text: 'Catalogue'),
              Tab(text: 'Available'),
              Tab(text: 'I have out'),
            ],
          ),
          const Expanded(
            child: TabBarView(
              children: [
                _CatalogueTab(),
                _AvailabilityTab(),
                _HoldingTab(),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Vocabulary — the server's stable codes, shown with their meaning.
// ---------------------------------------------------------------------------

/// Condition grades, best to worst — the server's own `CONDITIONS`.
const kConditionGrades = <String>['new', 'good', 'fair', 'poor', 'unserviceable'];

String conditionLabel(String grade) => switch (grade.toLowerCase()) {
      'new' => 'New',
      'good' => 'Good',
      'fair' => 'Fair',
      'poor' => 'Poor',
      'unserviceable' => 'Unserviceable',
      _ => grade,
    };

/// Why an item cannot be taken — the server's `reasons` codes in words.
///
/// The code is kept beside the sentence, because the code is the server's word
/// and a client that renames it can drift from the server that emits it.
String reasonLabel(String reason) => switch (reason) {
      'retired' => 'Retired — gone from the pool for good',
      'in_maintenance' => 'In maintenance — out of the pool for service',
      'unserviceable' => 'Unserviceable — not in the pool until its condition is fixed',
      'checked_out' => 'Checked out — somebody already has it across this window',
      _ => reason,
    };

String itemStatusLabel(String status) => switch (status.toLowerCase()) {
      'available' => 'In the pool',
      'maintenance' => 'In maintenance',
      'retired' => 'Retired',
      _ => status,
    };

/// `YYYY-MM-DD`, the shape every equipment date field and query takes.
String isoDate(DateTime date) =>
    '${date.year.toString().padLeft(4, '0')}-'
    '${date.month.toString().padLeft(2, '0')}-'
    '${date.day.toString().padLeft(2, '0')}';

DateTime _dateOnly(DateTime d) => DateTime(d.year, d.month, d.day);

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

class _CatalogueTab extends StatefulWidget {
  const _CatalogueTab();

  @override
  State<_CatalogueTab> createState() => _CatalogueTabState();
}

class _CatalogueTabState extends State<_CatalogueTab> {
  List<Map<String, dynamic>> _items = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;
  final _search = TextEditingController();
  bool _includeRetired = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void dispose() {
    _search.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final session = context.read<SessionState>();
    setState(() {
      _loading = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      final list = await session.cachedList(
        'equipment.items',
        () => session.api.equipmentItems(
          q: _search.text.trim().isEmpty ? null : _search.text.trim(),
          includeRetired: _includeRetired,
        ),
      );
      if (!mounted) return;
      setState(() {
        _items = list.value;
        _stale = list.isStale;
        _cachedAt = list.cachedAt;
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
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());

    if (_refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'The gear pool is not yours to read',
        message: 'Reading the catalogue needs equipment:read at troop scope.'
            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null && _items.isEmpty) {
      return EmptyState(
        icon: Icons.cloud_off,
        title: 'Cannot reach the server',
        message: _error!,
        action: FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Padding(
          padding: const EdgeInsets.fromLTRB(
            AppSpacing.md,
            AppSpacing.md,
            AppSpacing.md,
            0,
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              TextField(
                controller: _search,
                textInputAction: TextInputAction.search,
                onSubmitted: (_) => _load(),
                decoration: InputDecoration(
                  prefixIcon: const Icon(Icons.search),
                  hintText: 'Search by name or asset tag',
                  suffixIcon: _search.text.isEmpty
                      ? null
                      : IconButton(
                          icon: const Icon(Icons.clear),
                          tooltip: 'Clear',
                          onPressed: () {
                            _search.clear();
                            _load();
                          },
                        ),
                ),
              ),
              Wrap(
                spacing: AppSpacing.sm,
                children: [
                  FilterChip(
                    label: const Text('Include retired'),
                    selected: _includeRetired,
                    onSelected: (on) {
                      setState(() => _includeRetired = on);
                      _load();
                    },
                  ),
                ],
              ),
            ],
          ),
        ),
        Expanded(
          child: _items.isEmpty
              ? EmptyState(
                  icon: Icons.inventory_2_outlined,
                  title: 'Nothing in the gear pool',
                  message: _includeRetired
                      ? 'The catalogue is empty. A quartermaster adds items, and '
                          'they appear here with their condition and location.'
                      : 'No in-pool items match. Retired items are hidden unless '
                          'you ask for them.',
                )
              : RefreshIndicator(
                  onRefresh: _load,
                  child: ListView.separated(
                    padding: const EdgeInsets.all(AppSpacing.md),
                    itemCount: _items.length,
                    separatorBuilder: (_, _) => const SizedBox(height: AppSpacing.sm),
                    itemBuilder: (context, i) => EquipmentCard(
                      item: _items[i],
                      onTap: () => _open(_items[i]),
                    ),
                  ),
                ),
        ),
      ],
    );
  }

  Future<void> _open(Map<String, dynamic> item) async {
    final id = field(item, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => EquipmentItemScreen(id: id)),
    );
    if (mounted) await _load();
  }
}

// ---------------------------------------------------------------------------
// Availability — what can I take on these dates, and why not
// ---------------------------------------------------------------------------

class _AvailabilityTab extends StatefulWidget {
  const _AvailabilityTab();

  @override
  State<_AvailabilityTab> createState() => _AvailabilityTabState();
}

class _AvailabilityTabState extends State<_AvailabilityTab> {
  late DateTime _from = _dateOnly(DateTime.now());
  late DateTime _to = _from.add(const Duration(days: 7));

  Map<String, dynamic> _page = const {};
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;

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
      _errorStatus = null;
    });
    try {
      final cached = await session.cachedMap(
        'equipment.availability',
        () => session.api.equipmentAvailability(
          from: isoDate(_from),
          to: isoDate(_to),
        ),
      );
      if (!mounted) return;
      setState(() {
        _page = cached.value;
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
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  List<Map<String, dynamic>> _asMaps(Object? value) =>
      ((value as List?) ?? const [])
          .whereType<Map>()
          .map((e) => Map<String, dynamic>.from(e))
          .toList();

  Future<void> _pick(bool isFrom) async {
    final picked = await showDatePicker(
      context: context,
      initialDate: isFrom ? _from : _to,
      firstDate: DateTime(2020),
      lastDate: DateTime(2100),
    );
    if (picked == null || !mounted) return;
    setState(() {
      if (isFrom) {
        _from = _dateOnly(picked);
        if (_to.isBefore(_from)) _to = _from;
      } else {
        _to = _dateOnly(picked);
        if (_to.isBefore(_from)) _from = _to;
      }
    });
    await _load();
  }

  @override
  Widget build(BuildContext context) {
    final available = _asMaps(_page['available']);
    final unavailable = _asMaps(_page['unavailable']);
    final notice = field(_page, ['notice']);

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Padding(
          padding: const EdgeInsets.fromLTRB(
            AppSpacing.md,
            AppSpacing.md,
            AppSpacing.md,
            0,
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Wrap(
                spacing: AppSpacing.sm,
                runSpacing: AppSpacing.xs,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  OutlinedButton.icon(
                    onPressed: () => _pick(true),
                    icon: const Icon(Icons.event_outlined, size: 18),
                    label: Text('From ${formatDate(isoDate(_from))}'),
                  ),
                  OutlinedButton.icon(
                    onPressed: () => _pick(false),
                    icon: const Icon(Icons.event_available_outlined, size: 18),
                    label: Text('To ${formatDate(isoDate(_to))}'),
                  ),
                ],
              ),
              if (notice.isNotEmpty) ...[
                const SizedBox(height: AppSpacing.xs),
                Text(
                  notice,
                  style: AppText.bodySmall.copyWith(
                    color: Theme.of(context).colorScheme.outline,
                  ),
                ),
              ],
            ],
          ),
        ),
        Expanded(child: _body(available, unavailable)),
      ],
    );
  }

  Widget _body(
    List<Map<String, dynamic>> available,
    List<Map<String, dynamic>> unavailable,
  ) {
    if (_loading) return const Center(child: CircularProgressIndicator());
    if (_refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'Availability is not yours to read',
        message: 'Asking what is available needs equipment:read at troop scope.'
            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null) {
      return EmptyState(
        icon: Icons.cloud_off,
        title: 'Cannot reach the server',
        message: _error!,
        action: FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }

    return RefreshIndicator(
      onRefresh: _load,
      child: ListView(
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          _sectionHeader('Available', available.length),
          if (available.isEmpty)
            const _Quiet('Nothing in the pool is free across this window.')
          else
            for (final item in available) ...[
              EquipmentCard(item: item, onTap: () => _open(item)),
              const SizedBox(height: AppSpacing.sm),
            ],
          const SizedBox(height: AppSpacing.md),
          _sectionHeader('Not available', unavailable.length),
          if (unavailable.isEmpty)
            const _Quiet('Every item in the pool is free across this window.')
          else
            for (final entry in unavailable) ...[
              _UnavailableCard(
                entry: entry,
                onTap: () {
                  final item = entry['item'];
                  if (item is Map) {
                    _open(Map<String, dynamic>.from(item));
                  }
                },
              ),
              const SizedBox(height: AppSpacing.sm),
            ],
        ],
      ),
    );
  }

  Widget _sectionHeader(String label, int count) => Padding(
        padding: const EdgeInsets.only(bottom: AppSpacing.sm),
        child: Text('$label ($count)', style: AppText.titleMedium),
      );

  Future<void> _open(Map<String, dynamic> item) async {
    final id = field(item, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => EquipmentItemScreen(id: id)),
    );
    if (mounted) await _load();
  }
}

/// An item that cannot be taken, with every reason the server gave and, where
/// a checkout blocks it, who holds it and the promise it was given.
class _UnavailableCard extends StatelessWidget {
  const _UnavailableCard({required this.entry, required this.onTap});

  final Map<String, dynamic> entry;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final item = entry['item'] is Map
        ? Map<String, dynamic>.from(entry['item'] as Map)
        : <String, dynamic>{};
    final reasons = ((entry['reasons'] as List?) ?? const [])
        .map((e) => e.toString())
        .toList();
    final blocking = ((entry['blocking'] as List?) ?? const [])
        .whereType<Map>()
        .map((e) => Map<String, dynamic>.from(e))
        .toList();

    return AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          EquipmentTitleRow(item: item, trailing: itemStatusLabel(field(item, ['status'], fallback: 'available'))),
          const SizedBox(height: AppSpacing.sm),
          // The reasons are the useful part: each one in words, with the
          // server's own code beside it so the two cannot drift.
          for (final reason in reasons)
            Padding(
              padding: const EdgeInsets.only(bottom: 4),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Icon(Icons.block, size: 16, color: AppColors.error),
                  const SizedBox(width: AppSpacing.sm),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(reasonLabel(reason), style: AppText.bodyMedium),
                        Text(
                          reason,
                          style: AppText.bodySmall.copyWith(color: scheme.outline),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
            ),
          for (final block in blocking)
            Padding(
              padding: const EdgeInsets.only(top: 2),
              child: Text(
                _blockingSentence(block),
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            ),
        ],
      ),
    );
  }

  static String _blockingSentence(Map<String, dynamic> block) {
    final holder = field(block, ['held_by'], fallback: 'somebody');
    final due = field(block, ['due_on']);
    final overdue = block['overdue'] == true;
    final buffer = StringBuffer('Held by $holder');
    if (due.isNotEmpty) {
      buffer.write(overdue
          ? ' — overdue since ${formatDate(due)}'
          : ' — due back ${formatDate(due)}');
    } else if (block['open'] == true) {
      buffer.write(' — open checkout with no due date, so it blocks indefinitely');
    }
    return buffer.toString();
  }
}

class _Quiet extends StatelessWidget {
  const _Quiet(this.text);

  final String text;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.only(bottom: AppSpacing.sm),
        child: Text(
          text,
          style: AppText.bodyMedium.copyWith(
            color: Theme.of(context).colorScheme.onSurface.withValues(alpha: 0.6),
          ),
        ),
      );
}

// ---------------------------------------------------------------------------
// What I am holding
// ---------------------------------------------------------------------------

class _HoldingTab extends StatefulWidget {
  const _HoldingTab();

  @override
  State<_HoldingTab> createState() => _HoldingTabState();
}

class _HoldingTabState extends State<_HoldingTab> {
  List<Map<String, dynamic>> _checkouts = const [];
  String _today = '';
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;
  String _member = '';

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final session = context.read<SessionState>();
    _member = (session.user?['id'] as String?)?.trim() ?? '';
    setState(() {
      _loading = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      // Narrowed to the caller with the server's own vocabulary: `state=open`
      // and the caller's own member id. Without a member id there is nobody to
      // narrow to, and the screen says so rather than showing the troop's log.
      if (_member.isEmpty) {
        setState(() {
          _checkouts = const [];
          _loading = false;
        });
        return;
      }
      final cached = await session.cachedMap(
        'equipment.checkouts.open',
        () => session.api.equipmentCheckouts(
          state: 'open',
          member: _member,
        ),
      );
      if (!mounted) return;
      final page = cached.value;
      setState(() {
        _checkouts = ((page['checkouts'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _today = field(page, ['today']);
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
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());
    if (_refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'The checkout log is not yours to read',
        message: 'Seeing what you hold needs equipment:read at troop scope.'
            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null) {
      return EmptyState(
        icon: Icons.cloud_off,
        title: 'Cannot reach the server',
        message: _error!,
        action: FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }
    if (_member.isEmpty) {
      return const EmptyState(
        icon: Icons.person_off_outlined,
        title: 'No member to narrow to',
        message: 'The session has no member id, so this app cannot ask which '
            'checkouts are yours without showing the whole troop the log. Sign '
            'in again, or read the catalogue instead.',
      );
    }
    if (_checkouts.isEmpty) {
      return Column(
        children: [
          if (_stale) OfflineBanner(cachedAt: _cachedAt),
          const Expanded(
            child: EmptyState(
              icon: Icons.backpack_outlined,
              title: 'Nothing checked out',
              message: 'You are holding no gear. Gear you check out appears here '
                  'with its due date, so you know what to bring back.',
            ),
          ),
        ],
      );
    }

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        Expanded(
          child: RefreshIndicator(
            onRefresh: _load,
            child: ListView.separated(
              padding: const EdgeInsets.all(AppSpacing.md),
              itemCount: _checkouts.length,
              separatorBuilder: (_, _) => const SizedBox(height: AppSpacing.sm),
              itemBuilder: (context, i) => _HoldingCard(
                checkout: _checkouts[i],
                today: _today,
                onTap: () => _open(_checkouts[i]),
              ),
            ),
          ),
        ),
      ],
    );
  }

  Future<void> _open(Map<String, dynamic> checkout) async {
    final itemId = field(checkout, ['item_id']);
    if (itemId.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => EquipmentItemScreen(id: itemId)),
    );
    if (mounted) await _load();
  }
}

class _HoldingCard extends StatelessWidget {
  const _HoldingCard({
    required this.checkout,
    required this.today,
    required this.onTap,
  });

  final Map<String, dynamic> checkout;
  final String today;
  final VoidCallback onTap;

  /// Whether this open checkout is past its due date. The server computes
  /// overdue with a grace period on its own `today`; the screen compares the
  /// promise to the server's date, not the device clock, so the two agree.
  bool get _overdue {
    final due = field(checkout, ['due_on']);
    if (due.isEmpty || today.isEmpty) return false;
    final dueDate = DateTime.tryParse(due);
    final now = DateTime.tryParse(today);
    if (dueDate == null || now == null) return false;
    return dueDate.isBefore(DateTime(now.year, now.month, now.day));
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final due = field(checkout, ['due_on']);
    final mission = field(checkout, ['mission_id']);
    return AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  field(checkout, ['item_name'], fallback: 'Item'),
                  style: AppText.titleLarge,
                ),
              ),
              const SizedBox(width: AppSpacing.sm),
              if (_overdue) const StatusBadge('rejected', label: 'Overdue'),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Text(
            due.isEmpty
                ? 'No due date — an open checkout with no promise blocks the item'
                : _overdue
                    ? 'Was due ${formatDate(due)}'
                    : 'Due ${formatDate(due)}',
            style: AppText.bodyMedium.copyWith(
              color: _overdue ? AppColors.error : scheme.onSurface,
            ),
          ),
          const SizedBox(height: AppSpacing.xs),
          Wrap(
            spacing: AppSpacing.md,
            runSpacing: 4,
            children: [
              if (field(checkout, ['asset_tag']).isNotEmpty)
                Text(field(checkout, ['asset_tag']), style: AppText.bodySmall),
              _MetaText('Out since ${formatDate(field(checkout, ['checked_out_on']))}'),
              if (mission.isNotEmpty) _MetaText('Mission #$mission'),
            ],
          ),
        ],
      ),
    );
  }
}

class _MetaText extends StatelessWidget {
  const _MetaText(this.text);

  final String text;

  @override
  Widget build(BuildContext context) => Text(
        text,
        style: AppText.bodySmall.copyWith(
          color: Theme.of(context).colorScheme.outline,
        ),
      );
}

// ---------------------------------------------------------------------------
// A catalogue row, shared by the tabs
// ---------------------------------------------------------------------------

/// One physical thing: its name, its condition, its location and its status.
class EquipmentCard extends StatelessWidget {
  const EquipmentCard({super.key, required this.item, required this.onTap});

  final Map<String, dynamic> item;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final condition = field(item, ['condition'], fallback: 'good');
    return AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          EquipmentTitleRow(
            item: item,
            trailing: itemStatusLabel(field(item, ['status'], fallback: 'available')),
          ),
          const SizedBox(height: AppSpacing.xs),
          Wrap(
            spacing: AppSpacing.sm,
            runSpacing: AppSpacing.xs,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              StatusBadge('active', label: 'Condition: ${conditionLabel(condition)}'),
              if (field(item, ['category']).isNotEmpty)
                StatusBadge('open', label: field(item, ['category'])),
            ],
          ),
          if (field(item, ['description']).isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(field(item, ['description']), style: AppText.bodyMedium),
          ],
          if (field(item, ['location']).isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Text(
              field(item, ['location']),
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
        ],
      ),
    );
  }
}

/// The name and asset tag line, with the status on the right.
class EquipmentTitleRow extends StatelessWidget {
  const EquipmentTitleRow({super.key, required this.item, required this.trailing});

  final Map<String, dynamic> item;
  final String trailing;

  @override
  Widget build(BuildContext context) {
    final tag = field(item, ['asset_tag']);
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(field(item, ['name'], fallback: 'Unnamed item'), style: AppText.titleLarge),
              if (tag.isNotEmpty)
                Text(
                  tag,
                  style: AppText.bodySmall.copyWith(
                    color: Theme.of(context).colorScheme.outline,
                  ),
                ),
            ],
          ),
        ),
        const SizedBox(width: AppSpacing.sm),
        StatusBadge(field(item, ['status'], fallback: 'available'), label: trailing),
      ],
    );
  }
}
