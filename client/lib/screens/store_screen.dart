import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'store_item_screen.dart';
import 'store_orders_screen.dart';

/// The shop — the catalogue.
///
/// It lives in the shell's navigation because buying a patch is ordinary troop
/// work, not an administrative setting: a new scout needs to find the uniform
/// the same week they join.
///
/// Two facts this screen keeps apart, because the shop's whole money model
/// turns on them:
///
///  * **The price** is the shop's, and it is the item's price — never "free".
///  * **What a member pays** is a share of it, and anything not charged is a
///    draw on the scholarship fund. That is why no row here says "free" and no
///    row shows a struck-through price: a reduction is a subsidy from a real
///    fund, and the row that would hide it is the row that lies.
///
/// Everything rendered comes from `GET /api/store/items`, including each item's
/// whole scale. The client prices nothing.
class StoreScreen extends StatefulWidget {
  const StoreScreen({super.key});

  @override
  State<StoreScreen> createState() => _StoreScreenState();
}

class _StoreScreenState extends State<StoreScreen> {
  List<Map<String, dynamic>> _items = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;

  /// Rentals and products are one catalogue; this only narrows what is shown,
  /// and the narrowing is the server's own `kind` vocabulary.
  String? _kind;

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
      final cached = await session.cachedList(
        'store.items',
        () => session.api.storeItems(),
      );
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
  }

  /// A refusal is not a failure to retry: the server answered, and the answer
  /// is that this caller may not read the catalogue.
  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  List<Map<String, dynamic>> get _visible => _kind == null
      ? _items
      : _items.where((i) => field(i, ['kind']) == _kind).toList();

  Future<void> _open(Map<String, dynamic> item) async {
    final id = field(item, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => StoreItemScreen(id: id)),
    );
    if (mounted) await _load(silent: true);
  }

  Future<void> _openOrders() async {
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => const StoreOrdersScreen()),
    );
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) return const Center(child: CircularProgressIndicator());

    return Column(
      children: [
        if (_stale) OfflineBanner(cachedAt: _cachedAt),
        if (!_refused && _error == null) _header(),
        Expanded(
          child: _refused
              ? EmptyState(
                  icon: Icons.lock_outline,
                  title: 'The catalogue is not yours to read',
                  // The requirement in words, then the server's own message: a
                  // bare "forbidden" leaves the reader nothing to act on, and
                  // which roles hold a permission lives in a table this client
                  // cannot read.
                  message: 'Reading the shop needs store:read at any scope.'
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
                      ? _empty()
                      : RefreshIndicator(
                          onRefresh: _load,
                          child: ListView.separated(
                            padding: const EdgeInsets.all(AppSpacing.md),
                            itemCount: _visible.length + 1,
                            separatorBuilder: (_, _) =>
                                const SizedBox(height: AppSpacing.sm),
                            itemBuilder: (context, i) {
                              if (i == _visible.length) return _moneyNote();
                              final item = _visible[i];
                              return _ItemCard(
                                item: item,
                                onTap: () => _open(item),
                              );
                            },
                          ),
                        ),
        ),
      ],
    );
  }

  Widget _header() {
    final scheme = Theme.of(context).colorScheme;
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
                child: Text(
                  _items.isEmpty
                      ? 'The shop'
                      : '${_visible.length} item${_visible.length == 1 ? '' : 's'} for sale',
                  style: AppText.titleMedium,
                ),
              ),
              TextButton.icon(
                onPressed: _openOrders,
                icon: const Icon(Icons.receipt_long_outlined, size: 18),
                label: const Text('My orders'),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Wrap(
            spacing: AppSpacing.sm,
            children: [
              for (final option in const [
                ('All', null),
                ('Products', 'product'),
                ('Rentals', 'rental'),
              ])
                ChoiceChip(
                  label: Text(option.$1),
                  selected: _kind == option.$2,
                  onSelected: (_) => setState(() => _kind = option.$2),
                ),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Text(
            'Every price is the shop\'s own. What a member is not charged is a '
            'draw on the scholarship fund, so a reduction never appears here as '
            'a smaller price.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ],
      ),
    );
  }

  Widget _empty() => EmptyState(
        icon: Icons.storefront_outlined,
        title: _kind == null ? 'Nothing for sale yet' : 'No ${_kind}s in the shop',
        message: _kind == null
            ? 'The shop has no items. A shopkeeper adds them, and they appear '
                'here with their price and the whole sliding scale.'
            : 'This filter is the server\'s own kind vocabulary; nothing in the '
                'catalogue matches it.',
        action: _kind == null
            ? null
            : FilledButton.tonal(
                onPressed: () => setState(() => _kind = null),
                child: const Text('Show all'),
              ),
      );

  Widget _moneyNote() => Padding(
        padding: const EdgeInsets.only(top: AppSpacing.sm, bottom: AppSpacing.lg),
        child: Text(
          'An order records three figures: the price, what the member is charged '
          'and what the scholarship fund covers. The order screen states all '
          'three, because a charge is not a price and a subsidy is not free.',
          style: AppText.bodySmall.copyWith(
            color: Theme.of(context).colorScheme.outline,
          ),
        ),
      );
}

/// One thing the shop sells.
class _ItemCard extends StatelessWidget {
  const _ItemCard({required this.item, required this.onTap});

  final Map<String, dynamic> item;
  final VoidCallback onTap;

  String get _kind => field(item, ['kind'], fallback: 'product').toLowerCase();

  bool get _inactive => item['active'] == false;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final name = field(item, ['name'], fallback: 'Unnamed item');
    final category = field(item, ['category']);
    final sku = field(item, ['sku']);
    final description = field(item, ['description']);
    final price = int.tryParse(field(item, ['base_price_cents']));

    return AppCard(
      onTap: onTap,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(
                child: Text(name, style: AppText.titleLarge),
              ),
              const SizedBox(width: AppSpacing.sm),
              Text(
                formatCents(price),
                style: AppText.titleLarge.copyWith(color: scheme.onSurface),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.xs),
          Wrap(
            spacing: AppSpacing.sm,
            runSpacing: AppSpacing.xs,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              StatusBadge(_kind, label: _kind == 'rental' ? 'Rental' : 'Product'),
              if (category.isNotEmpty)
                StatusBadge('active', label: _titleCase(category)),
              if (_inactive) const StatusBadge('rejected', label: 'Inactive'),
              if (sku.isNotEmpty)
                Text(sku, style: AppText.bodySmall.copyWith(color: scheme.outline)),
            ],
          ),
          if (description.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(description, style: AppText.bodyMedium),
          ],
        ],
      ),
    );
  }

  static String _titleCase(String raw) =>
      raw.isEmpty ? raw : raw.substring(0, 1).toUpperCase() + raw.substring(1);
}
