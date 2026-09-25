import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'store_item_screen.dart';
import 'store_order_screen.dart';
import 'store_orders_screen.dart';

/// Shop administration — the operator's surfaces, behind Settings.
///
/// They live here rather than in the shell's navigation because they are not
/// daily scout work: the shell's destinations are what a scout needs on a
/// Friday, and these are what a shopkeeper or treasurer needs when something is
/// wrong. Three surfaces, one per permission the server enforces:
///
///  * **Unsettled** (`store:read_all`) — the worklist. An order awaiting payment
///    cannot be told from one this plugin cannot see paid, so both appear
///    together and the server's own note says so.
///  * **New item** (`store:manage`) — add something the shop sells.
///  * **Comps** (`store:read_all`) — every comp, its reason, its authority and
///    the draw it produced. The ledger shows the draw; this shows the comp.
///
/// None of the three guesses at the reader's roles. Each is offered, each states
/// the permission it needs in words, and each renders the server's own answer —
/// including its refusal, which is repeated rather than replaced.
class StoreAdminScreen extends StatelessWidget {
  const StoreAdminScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return DefaultTabController(
      length: 3,
      child: Scaffold(
        appBar: AppBar(
          title: const Text('The shop'),
          bottom: const TabBar(
            tabs: [
              Tab(text: 'Unsettled'),
              Tab(text: 'New item'),
              Tab(text: 'Comps'),
            ],
          ),
        ),
        body: const TabBarView(
          children: [
            _UnsettledTab(),
            _NewItemTab(),
            _CompsTab(),
          ],
        ),
      ),
    );
  }
}

/// The worklist: orders whose money has not landed.
class _UnsettledTab extends StatefulWidget {
  const _UnsettledTab();

  @override
  State<_UnsettledTab> createState() => _UnsettledTabState();
}

class _UnsettledTabState extends State<_UnsettledTab> {
  List<Map<String, dynamic>> _orders = const [];
  Map<String, dynamic> _reasons = const {};
  int _total = 0;
  int? _olderThan;
  String _note = '';
  bool _loading = true;
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
      final page = await session.api.unsettledStoreOrders();
      if (!mounted) return;
      setState(() {
        _orders = ((page['orders'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _reasons = (page['by_reason'] as Map?)?.cast<String, dynamic>() ?? const {};
        _total = (page['total_unsettled'] as num?)?.toInt() ?? _orders.length;
        _olderThan = (page['older_than_minutes'] as num?)?.toInt();
        _note = field(page, ['note']);
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

  Future<void> _open(Map<String, dynamic> order) async {
    final id = field(order, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => StoreOrderScreen(id: id)),
    );
    if (mounted) await _load();
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());
    final refused = _errorStatus == 401 || _errorStatus == 403;
    if (refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'The worklist is for operators',
        // The requirement in words, then the server's own message — the defect
        // this rule exists to prevent was a screen showing only "forbidden".
        message: 'The unsettled worklist needs store:read_all at troop scope. '
            'On a Lodge grant it is not readable; a troop-scope grant is what '
            'covers every Lodge.'
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
    final scheme = Theme.of(context).colorScheme;
    return RefreshIndicator(
      onRefresh: _load,
      child: ListView(
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Text(
                        _total == 0
                            ? 'Nothing unsettled'
                            : '$_total unsettled',
                        style: AppText.titleLarge,
                      ),
                    ),
                    if (_olderThan != null)
                      StatusBadge(
                        _total == 0 ? 'active' : 'review',
                        label: _olderThan == 0
                            ? 'no age threshold'
                            : 'older than ${_olderThan}m',
                      ),
                  ],
                ),
                const SizedBox(height: AppSpacing.sm),
                if (_total == 0)
                  Text(
                    'Every order has landed: nothing is awaiting payment past '
                    'the threshold, and no completed order has an unbooked draw '
                    'or an unconfirmed ledger entry.',
                    style: AppText.bodyMedium,
                  )
                else ...[
                  Wrap(
                    spacing: AppSpacing.sm,
                    runSpacing: AppSpacing.xs,
                    children: [
                      for (final entry in _reasons.entries)
                        StatusBadge(
                          'review',
                          label: '${_reasonLabel(entry.key)}: ${entry.value}',
                        ),
                    ],
                  ),
                  const SizedBox(height: AppSpacing.sm),
                  Text(
                    'Two shapes of "not settled" are counted together, because '
                    'from here they cannot be told apart.',
                    style: AppText.bodySmall.copyWith(color: scheme.outline),
                  ),
                ],
                if (_note.isNotEmpty) ...[
                  const SizedBox(height: AppSpacing.sm),
                  Text(
                    _note,
                    style: AppText.bodySmall.copyWith(color: scheme.outline),
                  ),
                ],
              ],
            ),
          ),
          const SizedBox(height: AppSpacing.md),
          for (final order in _orders) ...[
            _UnsettledCard(order: order, onTap: () => _open(order)),
            const SizedBox(height: AppSpacing.sm),
          ],
          const SizedBox(height: AppSpacing.sm),
          Text(
            'A shopkeeper completes a paid order against stripe\'s own record; a '
            'treasurer books an unbooked draw. Open an order to do either.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ],
      ),
    );
  }

  static String _reasonLabel(String reason) => switch (reason) {
        'awaiting_payment' => 'Awaiting payment',
        'draw_unbooked' => 'Draw not booked',
        'ledger_unconfirmed' => 'Ledger unconfirmed',
        _ => reason,
      };
}

class _UnsettledCard extends StatelessWidget {
  const _UnsettledCard({required this.order, required this.onTap});

  final Map<String, dynamic> order;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final reason = field(order, ['unsettled_reason']);
    final status = field(order, ['status'], fallback: 'open').toLowerCase();
    final price = int.tryParse(field(order, ['price_cents']));
    final charged = int.tryParse(field(order, ['charged_cents']));
    final funded = int.tryParse(field(order, ['funded_cents']));
    final draw = field(order, ['draw_status']);

    return AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  'Order #${field(order, ['id'])}',
                  style: AppText.titleLarge,
                ),
              ),
              StatusBadge(status, label: storeStatusLabel(status)),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Text(
            _UnsettledTabState._reasonLabel(reason),
            style: AppText.titleMedium.copyWith(color: AppColors.warning),
          ),
          const SizedBox(height: AppSpacing.xs),
          Text(
            'Price ${formatCents(price)} · charged ${formatCents(charged)} · '
            'funded ${formatCents(funded)}'
            '${draw.isEmpty ? '' : ' · draw $draw'}',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
          const SizedBox(height: AppSpacing.xs),
          Text(
            'Member ${field(order, ['member_id'])} · placed '
            '${formatDate(field(order, ['created_at']), withTime: true)}',
            style: AppText.bodySmall,
          ),
        ],
      ),
    );
  }
}

/// Add something the shop sells. `store:manage`.
class _NewItemTab extends StatefulWidget {
  const _NewItemTab();

  @override
  State<_NewItemTab> createState() => _NewItemTabState();
}

class _NewItemTabState extends State<_NewItemTab> {
  /// The server's own category vocabulary (docs/api-reference.md, Store). The
  /// server validates it again; this is the list it accepts.
  static const _categories = [
    'uniform',
    'patch',
    'insignia',
    'gear',
    'merch',
    'other',
  ];

  final _name = TextEditingController();
  final _price = TextEditingController();
  final _sku = TextEditingController();
  final _description = TextEditingController();
  final _fund = TextEditingController();
  final _equipment = TextEditingController();

  String _kind = 'product';
  String _category = 'uniform';
  bool _busy = false;
  String? _error;

  @override
  void dispose() {
    for (final controller in [
      _name,
      _price,
      _sku,
      _description,
      _fund,
      _equipment,
    ]) {
      controller.dispose();
    }
    super.dispose();
  }

  /// Dollars and cents, as a person types them, turned into the cents the API
  /// takes. Nothing here prices anything — this only converts the input.
  int? get _priceCents {
    final raw = _price.text.trim().replaceAll(',', '');
    if (raw.isEmpty) return null;
    final value = double.tryParse(raw);
    if (value == null || value < 0) return null;
    return (value * 100).round();
  }

  Future<void> _create() async {
    final session = context.read<SessionState>();
    final name = _name.text.trim();
    if (name.isEmpty) {
      setState(() => _error = 'A name is required.');
      return;
    }
    final price = _priceCents;
    if (price == null) {
      setState(() => _error = 'A base price in dollars is required — for '
          'example 25.00. The shop charges a share of it, never above it.');
      return;
    }
    final equipmentId = int.tryParse(_equipment.text.trim());
    if (_kind == 'rental' && equipmentId == null) {
      setState(() => _error = 'A rental must name the equipment item it rents: '
          'custody and condition stay equipment\'s, and the shop holds the id '
          'and the fee.');
      return;
    }
    if (_kind == 'product' && equipmentId != null) {
      setState(() => _error = 'A product may not name an equipment item: '
          'renting is kind "rental", and custody stays in equipment\'s crate.');
      return;
    }

    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final response = await session.api.createStoreItem(
        kind: _kind,
        name: name,
        category: _category,
        basePriceCents: price,
        sku: _sku.text,
        description: _description.text,
        fundCode: _fund.text,
        equipmentItemId: equipmentId,
      );
      if (!mounted) return;
      final item = (response['item'] as Map?)?.cast<String, dynamic>() ?? const {};
      for (final controller in [_name, _price, _sku, _description, _equipment]) {
        controller.clear();
      }
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Added ${field(item, ['name'], fallback: name)} to the shop.')),
      );
      final id = field(item, ['id']);
      if (id.isNotEmpty) {
        await Navigator.of(context).push(
          MaterialPageRoute<void>(builder: (_) => StoreItemScreen(id: id)),
        );
      }
    } on ApiException catch (e) {
      if (!mounted) return;
      // The refusal names its requirement and keeps the server's words: the two
      // together are what an operator can act on.
      setState(() => _error = e.statusCode == 403
          ? 'Adding an item needs store:manage at troop scope. '
              'The server said: ${e.message}'
          : e.message);
    } on Object catch (e) {
      if (!mounted) return;
      setState(() => _error = 'Cannot reach the server — nothing was added.');
      debugPrint('create item failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        Text(
          'Adding an item needs store:manage at troop scope. The price is the '
          'shop\'s; each tier pays a share of it, and anything not charged is a '
          'draw on the scholarship fund — so there is no way to enter a '
          '"free" item here, because a zero price and a comp are different '
          'things.',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
        const SizedBox(height: AppSpacing.md),
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('Kind', style: AppText.titleMedium),
              const SizedBox(height: AppSpacing.xs),
              Wrap(
                spacing: AppSpacing.sm,
                children: [
                  for (final option in const ['product', 'rental'])
                    ChoiceChip(
                      label: Text(option == 'rental' ? 'Rental' : 'Product'),
                      selected: _kind == option,
                      onSelected: (_) => setState(() => _kind = option),
                    ),
                ],
              ),
              const SizedBox(height: AppSpacing.md),
              TextField(
                controller: _name,
                decoration: const InputDecoration(labelText: 'Name'),
              ),
              const SizedBox(height: AppSpacing.md),
              Text('Category', style: AppText.titleMedium),
              const SizedBox(height: AppSpacing.xs),
              Wrap(
                spacing: AppSpacing.sm,
                runSpacing: AppSpacing.xs,
                children: [
                  for (final category in _categories)
                    ChoiceChip(
                      label: Text(category),
                      selected: _category == category,
                      onSelected: (_) => setState(() => _category = category),
                    ),
                ],
              ),
              const SizedBox(height: AppSpacing.md),
              TextField(
                controller: _price,
                keyboardType: const TextInputType.numberWithOptions(decimal: true),
                decoration: const InputDecoration(
                  labelText: 'Base price, in dollars',
                  helperText: 'The shop\'s own price — the sale\'s value, '
                      'never a member\'s charge',
                  prefixText: r'$ ',
                ),
              ),
              const SizedBox(height: AppSpacing.md),
              TextField(
                controller: _sku,
                decoration: const InputDecoration(labelText: 'SKU (optional)'),
              ),
              const SizedBox(height: AppSpacing.md),
              TextField(
                controller: _description,
                maxLines: 3,
                decoration:
                    const InputDecoration(labelText: 'Description (optional)'),
              ),
              const SizedBox(height: AppSpacing.md),
              TextField(
                controller: _fund,
                decoration: const InputDecoration(
                  labelText: 'Fund code (optional)',
                  helperText: 'Where the order\'s proceeds land; blank uses the '
                      'shop\'s default',
                ),
              ),
              if (_kind == 'rental') ...[
                const SizedBox(height: AppSpacing.md),
                TextField(
                  controller: _equipment,
                  keyboardType: TextInputType.number,
                  decoration: const InputDecoration(
                    labelText: 'Equipment item id',
                    helperText: 'The id only — no name, no condition: custody '
                        'stays equipment\'s',
                  ),
                ),
              ],
              if (_error != null) ...[
                const SizedBox(height: AppSpacing.md),
                Text(
                  _error!,
                  style: AppText.bodySmall.copyWith(color: scheme.error),
                ),
              ],
              const SizedBox(height: AppSpacing.md),
              if (_busy)
                const SizedBox(
                  width: 24,
                  height: 24,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              else
                FilledButton.icon(
                  onPressed: _create,
                  icon: const Icon(Icons.add, size: 20),
                  label: const Text('Add the item'),
                ),
            ],
          ),
        ),
        const SizedBox(height: AppSpacing.lg),
        Text(
          'Patching an existing item is not offered here yet: the server has the '
          'route (PATCH /api/store/item/{id}) and this surface does not. What it '
          'does not do, it does not pretend to.',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
        const SizedBox(height: AppSpacing.lg),
      ],
    );
  }
}

/// Every comp, with its reason, its authority and the draw it produced.
class _CompsTab extends StatefulWidget {
  const _CompsTab();

  @override
  State<_CompsTab> createState() => _CompsTabState();
}

class _CompsTabState extends State<_CompsTab> {
  List<Map<String, dynamic>> _comps = const [];
  int? _fundedTotal;
  String _note = '';
  bool _loading = true;
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
      final page = await session.api.storeComps();
      if (!mounted) return;
      setState(() {
        _comps = ((page['comps'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _fundedTotal = int.tryParse(field(page, ['funded_total_cents']));
        _note = field(page, ['note']);
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

  Future<void> _open(Map<String, dynamic> comp) async {
    final id = field(comp, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => StoreOrderScreen(id: id)),
    );
    if (mounted) await _load();
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());
    if (_errorStatus == 401 || _errorStatus == 403) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'Comps are for operators',
        message: 'Reading every comp needs store:read_all at troop scope.'
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
    final scheme = Theme.of(context).colorScheme;
    return RefreshIndicator(
      onRefresh: _load,
      child: ListView(
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Text(
                        _comps.isEmpty
                            ? 'No comps'
                            : '${_comps.length} comp${_comps.length == 1 ? '' : 's'}',
                        style: AppText.titleLarge,
                      ),
                    ),
                    if (_fundedTotal != null)
                      Text(
                        'Funded ${formatCents(_fundedTotal)}',
                        style: AppText.titleMedium
                            .copyWith(color: AppColors.warning),
                      ),
                  ],
                ),
                const SizedBox(height: AppSpacing.sm),
                Text(
                  'The ledger shows the draw; this shows the comp. A comp '
                  'charged nothing and drew its whole price from the scholarship '
                  'fund — finance cannot hold a zero-amount transaction, so the '
                  'positive transfer is what the ledger records.',
                  style: AppText.bodyMedium,
                ),
                if (_note.isNotEmpty) ...[
                  const SizedBox(height: AppSpacing.sm),
                  Text(
                    _note,
                    style: AppText.bodySmall.copyWith(color: scheme.outline),
                  ),
                ],
              ],
            ),
          ),
          const SizedBox(height: AppSpacing.md),
          for (final comp in _comps) ...[
            AppCard(
              onTap: () => _open(comp),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    children: [
                      Expanded(
                        child: Text(
                          'Order #${field(comp, ['id'])}',
                          style: AppText.titleLarge,
                        ),
                      ),
                      StatusBadge('rejected', label: 'Comped'),
                    ],
                  ),
                  const SizedBox(height: AppSpacing.xs),
                  Text(
                    field(comp, ['comp_reason'], fallback: 'no reason recorded'),
                    style: AppText.bodyMedium,
                  ),
                  const SizedBox(height: AppSpacing.xs),
                  Text(
                    'Price ${formatCents(int.tryParse(field(comp, ['price_cents'])))} · '
                    'charged ${formatCents(int.tryParse(field(comp, ['charged_cents'])))} · '
                    'funded ${formatCents(int.tryParse(field(comp, ['funded_cents'])))}',
                    style: AppText.bodySmall.copyWith(color: scheme.outline),
                  ),
                  const SizedBox(height: 2),
                  Text(
                    'By ${field(comp, ['comp_by'])} · '
                    '${formatDate(field(comp, ['comp_at']), withTime: true)}'
                    '${field(comp, ['draw_status']).isEmpty ? '' : ' · draw ${field(comp, ['draw_status'])}'}',
                    style: AppText.bodySmall,
                  ),
                ],
              ),
            ),
            const SizedBox(height: AppSpacing.sm),
          ],
          const SizedBox(height: AppSpacing.md),
          Text(
            'The ledger and this list are reconciled by hand: the draw is '
            'finance\'s entry, the comp is the shop\'s record of the authority '
            'that forgave the charge.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
          const SizedBox(height: AppSpacing.lg),
        ],
      ),
    );
  }
}
