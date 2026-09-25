import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import '../widgets/store_money.dart';
import 'store_order_screen.dart';

/// One item in the shop, and the two things a buyer needs: what it costs, and
/// what they will actually be charged.
///
/// The price is the shop's; the charge is the **tier's share of it**, capped at
/// the price, and the difference is a draw on the scholarship fund. This screen
/// shows the whole scale — every tier's charge *and* its draw — because the
/// scale is exactly the thing a sliding-scale shop has to make visible. It comes
/// from `GET /api/store/item/{id}`; the client computes none of it.
///
/// The buy action places an order and nothing more: `POST /api/store/order`
/// prices it from the catalogue and answers with the order, its lines, its draw
/// and the server's own `next` sentence. Paying is a separate step, offered on
/// the order screen where the charged figure and the draw both live.
class StoreItemScreen extends StatefulWidget {
  const StoreItemScreen({super.key, required this.id});

  final String id;

  @override
  State<StoreItemScreen> createState() => _StoreItemScreenState();
}

class _StoreItemScreenState extends State<StoreItemScreen> {
  Map<String, dynamic> _item = const {};
  bool _loading = true;
  bool _busy = false;
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
      final item = await session.api.storeItem(widget.id);
      if (!mounted) return;
      setState(() {
        _item = item;
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

  List<Map<String, dynamic>> get _scale =>
      ((_item['scale'] as List?) ?? const [])
          .whereType<Map>()
          .map((e) => Map<String, dynamic>.from(e))
          .toList();

  bool get _inactive => _item['active'] == false;

  String get _kind => field(_item, ['kind'], fallback: 'product').toLowerCase();

  Future<void> _buy() async {
    final choice = await showModalBottomSheet<_BuyChoice>(
      context: context,
      isScrollControlled: true,
      backgroundColor: Theme.of(context).colorScheme.surface,
      builder: (context) => _BuySheet(
        name: field(_item, ['name'], fallback: 'this item'),
        priceCents: int.tryParse(field(_item, ['base_price_cents'])),
        scale: _scale,
      ),
    );
    if (choice == null || !mounted) return;
    await _place(choice);
  }

  Future<void> _place(_BuyChoice choice) async {
    final session = context.read<SessionState>();
    setState(() => _busy = true);
    try {
      final response = await session.api.placeStoreOrder(
        lines: [
          {'item_id': int.tryParse(field(_item, ['id'])) ?? 0, 'quantity': choice.quantity},
        ],
        tier: choice.tier,
      );
      if (!mounted) return;
      final order = (response['order'] as Map?)?.cast<String, dynamic>() ?? const {};
      final orderId = field(order, ['id']);
      final next = field(response, ['next']);
      if (orderId.isEmpty) {
        _say('The order was placed but the server named no order id.', bad: true);
        return;
      }
      _say(next.isEmpty ? 'Order placed.' : next);
      await Navigator.of(context).push(
        MaterialPageRoute<void>(builder: (_) => StoreOrderScreen(id: orderId)),
      );
    } on ApiException catch (e) {
      if (!mounted) return;
      // Ordering needs store:buy, and a refusal is the server's own sentence —
      // stated with the requirement beside it rather than as a bare 403.
      _say(
        e.statusCode == 403
            ? 'Placing an order needs store:buy at any scope. The server said: ${e.message}'
            : e.message,
        bad: true,
      );
    } on Object catch (e) {
      if (!mounted) return;
      _say('Cannot reach the server — no order was placed.', bad: true);
      debugPrint('place order failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
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
        title: Text(field(_item, ['name'], fallback: 'Item')),
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
                  title: 'Not yours to read',
                  message: 'Reading a catalogue item needs store:read at any '
                      'scope.'
                      '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
                )
              : _item.isEmpty
                  ? EmptyState(
                      icon: Icons.cloud_off,
                      title: 'Cannot reach the server',
                      message: _error ?? 'Nothing came back for this item.',
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
    final price = int.tryParse(field(_item, ['base_price_cents']));
    final category = field(_item, ['category']);
    final sku = field(_item, ['sku']);
    final description = field(_item, ['description']);
    final fund = field(_item, ['fund_code']);
    final custody = (_item['custody'] as Map?)?.cast<String, dynamic>();

    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                field(_item, ['name'], fallback: 'Unnamed item'),
                style: AppText.headlineMedium,
              ),
            ),
            const SizedBox(width: AppSpacing.md),
            Text(formatCents(price), style: AppText.headlineMedium),
          ],
        ),
        const SizedBox(height: AppSpacing.sm),
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
          const SizedBox(height: AppSpacing.md),
          Text(description, style: AppText.bodyLarge),
        ],
        const SizedBox(height: AppSpacing.md),
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('The price', style: AppText.titleLarge),
              const SizedBox(height: AppSpacing.sm),
              Text(
                '${formatCents(price)} is what the goods cost. A member pays the '
                'share their tier carries, capped at this price, so a patron '
                'never pays the shop more than the shop asked.',
                style: AppText.bodyMedium,
              ),
              if (fund.isNotEmpty) ...[
                const SizedBox(height: AppSpacing.xs),
                Text(
                  'Proceeds land in the $fund fund; any draw leaves the '
                  'scholarship fund and enters that one.',
                  style: AppText.bodySmall.copyWith(color: scheme.outline),
                ),
              ],
            ],
          ),
        ),
        const SizedBox(height: AppSpacing.md),
        _scaleCard(),
        if (custody != null) ...[
          const SizedBox(height: AppSpacing.md),
          _custodyCard(custody),
        ],
        const SizedBox(height: AppSpacing.lg),
        if (_busy)
          const Center(
            child: SizedBox(
              width: 24,
              height: 24,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
          )
        else
          FilledButton.icon(
            onPressed: _inactive ? null : _buy,
            icon: const Icon(Icons.shopping_bag_outlined, size: 20),
            label: Text(_inactive ? 'Not for sale' : 'Buy'),
          ),
        if (_inactive) ...[
          const SizedBox(height: AppSpacing.sm),
          Text(
            'This item is inactive: the server will not sell it, and this button '
            'is disabled rather than offering an order that would be refused.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ],
        const SizedBox(height: AppSpacing.lg),
        Text(
          'Placing an order does not take money. The shop prices it from the '
          'catalogue; paying is a separate step, and it charges the amount the '
          'order records as charged — never the price.',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
      ],
    );
  }

  /// The whole sliding scale, with each tier's charge **and** its draw.
  Widget _scaleCard() {
    final scheme = Theme.of(context).colorScheme;
    if (_scale.isEmpty) {
      return AppCard(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('The sliding scale', style: AppText.titleLarge),
            const SizedBox(height: AppSpacing.sm),
            Text(
              'The server did not state a scale for this item, so there is '
              'nothing to show — this client will not invent charges.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            ),
          ],
        ),
      );
    }
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('The sliding scale', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          Text(
            'Each tier is a share of the price. What a tier does not charge is a '
            'draw on the scholarship fund — a real subsidy from a real fund.',
            style: AppText.bodyMedium,
          ),
          const SizedBox(height: AppSpacing.md),
          for (final row in _scale) _tierRow(row, scheme),
        ],
      ),
    );
  }

  Widget _tierRow(Map<String, dynamic> row, ColorScheme scheme) {
    final charged = int.tryParse(field(row, ['charged_cents']));
    final funded = int.tryParse(field(row, ['funded_cents']));
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.sm),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(
                child: Text(
                  field(row, ['label'], fallback: field(row, ['tier'])),
                  style: AppText.titleMedium,
                ),
              ),
              const SizedBox(width: AppSpacing.sm),
              Text(
                'pays ${formatCents(charged)}',
                style: AppText.titleMedium.copyWith(color: scheme.onSurface),
              ),
            ],
          ),
          const SizedBox(height: 2),
          Text(
            (funded ?? 0) > 0
                ? '${formatCents(funded)} drawn from the scholarship fund'
                    '${row['capped_at_price'] == true ? ' · share capped at the price' : ''}'
                : 'nothing drawn — this tier pays the whole price',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
          const SizedBox(height: 2),
          Text(field(row, ['description']), style: AppText.bodySmall),
        ],
      ),
    );
  }

  /// A rental's item lives in equipment, and this screen says so rather than
  /// copying a name or a condition it does not own.
  Widget _custodyCard(Map<String, dynamic> custody) {
    final scheme = Theme.of(context).colorScheme;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              const Icon(Icons.inventory_2_outlined, size: 20),
              const SizedBox(width: AppSpacing.sm),
              Expanded(
                child: Text('Custody belongs to equipment',
                    style: AppText.titleLarge),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          Text(
            'This shop sells the rental fee and holds the equipment item id '
            '(${field(custody, ['equipment_item_id'])}). The item, its condition '
            'and the checkout state machine are equipment\'s, at its own routes:',
            style: AppText.bodyMedium,
          ),
          const SizedBox(height: AppSpacing.sm),
          for (final route in [
            field(custody, ['availability']),
            field(custody, ['checkout']),
          ])
            if (route.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.xs),
                child: SelectableText(
                  route,
                  style: AppText.bodySmall.copyWith(color: scheme.outline),
                ),
              ),
          if (field(custody, ['note']).isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              field(custody, ['note']),
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
        ],
      ),
    );
  }

  static String _titleCase(String raw) =>
      raw.isEmpty ? raw : raw.substring(0, 1).toUpperCase() + raw.substring(1);
}

/// What the buyer chose in the sheet: how many, and at which tier.
class _BuyChoice {
  const _BuyChoice(this.quantity, this.tier);

  final int quantity;
  final String? tier;
}

/// The buy sheet — quantity and tier, each row a 56dp target for cold hands.
///
/// There is no amount field: the shop prices from its catalogue, and a
/// client-side price would be a second answer to a question only the server may
/// answer.
class _BuySheet extends StatefulWidget {
  const _BuySheet({
    required this.name,
    required this.priceCents,
    required this.scale,
  });

  final String name;
  final int? priceCents;
  final List<Map<String, dynamic>> scale;

  @override
  State<_BuySheet> createState() => _BuySheetState();
}

class _BuySheetState extends State<_BuySheet> {
  int _quantity = 1;

  /// The tier chosen. Starts on `standard` when the server offers it — the tier
  /// that pays the whole price — because a reduction is the buyer's to claim,
  /// not this screen's to assume.
  String? _tier;

  @override
  void initState() {
    super.initState();
    for (final row in widget.scale) {
      if (field(row, ['tier']) == 'standard') {
        _tier = 'standard';
        break;
      }
    }
    if (_tier == null && widget.scale.isNotEmpty) {
      _tier = field(widget.scale.first, ['tier']);
    }
  }

  int? get _chargedForTier {
    for (final row in widget.scale) {
      if (field(row, ['tier']) == _tier) {
        return int.tryParse(field(row, ['charged_cents']));
      }
    }
    return widget.priceCents;
  }

  int? get _fundedForTier {
    for (final row in widget.scale) {
      if (field(row, ['tier']) == _tier) {
        return int.tryParse(field(row, ['funded_cents']));
      }
    }
    return 0;
  }

  /// One sentence describing the chosen tier, using the server's own figures.
  String _selectedTierNote() {
    for (final row in widget.scale) {
      if (field(row, ['tier']) != _tier) continue;
      final charged = int.tryParse(field(row, ['charged_cents']));
      final funded = int.tryParse(field(row, ['funded_cents'])) ?? 0;
      final share = field(row, ['share_percent']);
      final label = field(row, ['label'], fallback: field(row, ['tier']));
      if (funded <= 0) {
        return '$label pays the shop\'s whole price'
            '${share.isEmpty ? '' : ' ($share of it)'} — nothing is drawn.';
      }
      return '$label pays ${formatCents(charged)}'
          '${share.isEmpty ? '' : ' ($share of the price)'} and draws '
          '${formatCents(funded)} from the scholarship fund.';
    }
    return 'No tier selected: the order will be priced at the shop\'s default.';
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final charged = _chargedForTier;
    final funded = _fundedForTier;
    return SafeArea(
      child: ListView(
        shrinkWrap: true,
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          Text('Buy ${widget.name}', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          Text(
            'The shop prices this order from its catalogue: ${formatCents(widget.priceCents)} '
            'each at the shop\'s price, and your tier\'s share is what is charged.',
            style: AppText.bodyMedium,
          ),
          const SizedBox(height: AppSpacing.md),
          Row(
            children: [
              Expanded(child: Text('Quantity', style: AppText.titleMedium)),
              IconButton.filledTonal(
                tooltip: 'One fewer',
                onPressed: _quantity > 1
                    ? () => setState(() => _quantity -= 1)
                    : null,
                icon: const Icon(Icons.remove),
              ),
              Padding(
                padding: const EdgeInsets.symmetric(horizontal: AppSpacing.md),
                child: Text('$_quantity', style: AppText.titleLarge),
              ),
              IconButton.filledTonal(
                tooltip: 'One more',
                onPressed: () => setState(() => _quantity += 1),
                icon: const Icon(Icons.add),
              ),
            ],
          ),
          if (widget.scale.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text('Tier', style: AppText.titleMedium),
            const SizedBox(height: AppSpacing.xs),
            Wrap(
              spacing: AppSpacing.sm,
              runSpacing: AppSpacing.xs,
              children: [
                for (final row in widget.scale)
                  ChoiceChip(
                    label: Text(
                      field(row, ['label'], fallback: field(row, ['tier'])),
                    ),
                    selected: _tier == field(row, ['tier']),
                    onSelected: (_) =>
                        setState(() => _tier = field(row, ['tier'])),
                  ),
              ],
            ),
            const SizedBox(height: AppSpacing.xs),
            Text(
              _selectedTierNote(),
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
          const SizedBox(height: AppSpacing.md),
          if (charged != null) ...[
            StoreMoneyRow(
              label: 'Price',
              what: 'what the goods cost, before any tier share',
              cents: widget.priceCents == null
                  ? null
                  : widget.priceCents! * _quantity,
              emphasis: scheme.onSurface,
            ),
            StoreMoneyRow(
              label: 'Charged',
              what: 'what this order will ask you to pay',
              cents: charged * _quantity,
              emphasis: charged > 0 ? AppColors.primary : scheme.onSurface,
            ),
            StoreMoneyRow(
              label: 'Funded',
              what: 'the draw on the scholarship fund for this purchase',
              cents: (funded ?? 0) * _quantity,
              emphasis: (funded ?? 0) > 0 ? AppColors.warning : AppColors.success,
            ),
            if ((funded ?? 0) > 0 && charged == 0) ...[
              const SizedBox(height: AppSpacing.xs),
              Text(
                'At this tier you pay nothing and the scholarship fund covers the '
                'whole price. If the order comes out with no charge, the server '
                'will point at /comp: a comp records its reason and the authority '
                'who granted it, because a comp is an authority rather than a '
                'price of zero.',
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            ],
          ],
          const SizedBox(height: AppSpacing.sm),
          FilledButton.icon(
            onPressed: () => Navigator.of(context).pop(
              _BuyChoice(_quantity, _tier),
            ),
            icon: const Icon(Icons.shopping_bag_outlined, size: 20),
            label: const Text('Place the order'),
          ),
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Cancel'),
          ),
        ],
      ),
    );
  }
}
