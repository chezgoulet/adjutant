import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import '../widgets/store_money.dart';
import 'store_orders_screen.dart';

/// One order, opened — and the screen where the shop's money model has to be
/// told straight.
///
/// An order carries three figures, and they are usually three different numbers:
/// the **price** (what the goods cost), what the member was **charged** (a share
/// of it, or zero), and what the **scholarship fund funded** (the difference).
/// This screen never presents the charged amount as though it were the price and
/// never presents a funded amount as though it were free money: each figure is
/// labelled, and the draw section says where the money actually sits.
///
/// The draw is a **machine-originated money movement that may be retried**, so it
/// has a state — `unbooked`, `attempting`, `booked` — and this screen shows it as
/// the server states it. A funded amount with an unbooked draw is money that has
/// not landed; saying so is the whole point of the worklist it appears on.
///
/// Every action is offered and the **server decides**: a refusal is rendered as
/// the permission it needs, in words, together with the server's own message.
/// This screen does not guess which roles hold what — that lives in a table it
/// cannot read.
class StoreOrderScreen extends StatefulWidget {
  const StoreOrderScreen({super.key, required this.id});

  final String id;

  @override
  State<StoreOrderScreen> createState() => _StoreOrderScreenState();
}

class _StoreOrderScreenState extends State<StoreOrderScreen> {
  Map<String, dynamic> _order = const {};
  Map<String, dynamic> _draw = const {};
  Map<String, dynamic> _ledger = const {};
  List<Map<String, dynamic>> _lines = const [];

  /// The Checkout session's own URL, once the server has opened one for this
  /// order. Kept here so the link survives a rebuild and can be copied.
  String _checkoutUrl = '';

  bool _loading = true;
  bool _busy = false;
  String? _error;
  int? _errorStatus;

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
      final data = await session.api.storeOrder(widget.id);
      if (!mounted) return;
      setState(() {
        _order = (data['order'] as Map?)?.cast<String, dynamic>() ?? const {};
        _draw = (data['draw'] as Map?)?.cast<String, dynamic>() ?? const {};
        _ledger = (data['ledger'] as Map?)?.cast<String, dynamic>() ?? const {};
        _lines = ((data['lines'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
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

  String get _status => field(_order, ['status'], fallback: 'open').toLowerCase();

  int? get _charged => int.tryParse(field(_order, ['charged_cents']));

  int? get _funded => int.tryParse(field(_order, ['funded_cents']));

  bool get _drawBooked => field(_draw, ['status']).toLowerCase() == 'booked';

  // --- actions ------------------------------------------------------------

  Future<void> _run(
    Future<Map<String, dynamic>> Function() action, {
    required String success,
  }) async {
    setState(() => _busy = true);
    try {
      final response = await action();
      if (!mounted) return;
      final url = field(response, ['checkout_url']);
      if (url.isNotEmpty) setState(() => _checkoutUrl = url);
      final next = field(response, ['next']);
      _say(next.isEmpty ? success : next);
      await _load(silent: true);
    } on ApiException catch (e) {
      if (!mounted) return;
      _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      _say('Cannot reach the server — nothing was changed.', bad: true);
      debugPrint('store order action failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  /// Open a Checkout session for the order's **charged** amount.
  ///
  /// The server forwards this caller's own credential to stripe, whose gate
  /// re-decides; when it refuses, stripe's words come back and are shown.
  Future<void> _checkout() =>
      _run(() => context.read<SessionState>().api.checkoutStoreOrder(widget.id),
          success: 'Checkout session opened.');

  /// Complete a paid order against stripe's own record.
  Future<void> _complete() async {
    final controller = TextEditingController();
    final id = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Complete this order'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text(
              'Completing checks the payment against stripe as you, then asks '
              'stripe to hand it to finance — so the ledger write and the '
              'completion land in one flow. The payment must be this order\'s: '
              'a payment for another amount is refused and nothing is written.',
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: controller,
              keyboardType: TextInputType.number,
              decoration: const InputDecoration(
                labelText: 'Stripe payment id',
                helperText: 'Stripe\'s own numeric payment id (pi_/cs_ row id)',
              ),
            ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(controller.text.trim()),
            child: const Text('Complete'),
          ),
        ],
      ),
    );
    if (id == null || id.isEmpty || !mounted) return;
    final paymentId = int.tryParse(id);
    if (paymentId == null) {
      _say('A stripe payment id is a number; "$id" is not one.', bad: true);
      return;
    }
    await _run(
      () => context
          .read<SessionState>()
          .api
          .completeStoreOrder(widget.id, stripePaymentId: paymentId),
      success: 'Order completed.',
    );
  }

  /// Comp the order: no charge, the whole price drawn from scholarship, with a
  /// mandatory reason and the authority recorded.
  Future<void> _comp() async {
    final controller = TextEditingController();
    final reason = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Comp this order'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text(
              'A comp charges nothing and draws the whole price from the '
              'scholarship fund. It is an authority, not a price of zero: the '
              'reason is required and is recorded against your name.',
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: controller,
              autofocus: true,
              maxLines: 2,
              decoration: const InputDecoration(
                labelText: 'Reason',
                helperText: 'Recorded on the order and in the audit log',
              ),
            ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () =>
                Navigator.of(context).pop(controller.text.trim()),
            child: const Text('Comp it'),
          ),
        ],
      ),
    );
    if (reason == null || reason.isEmpty || !mounted) return;
    await _run(
      () => context
          .read<SessionState>()
          .api
          .compStoreOrder(widget.id, reason: reason),
      success: 'Order comped.',
    );
  }

  /// Book the outstanding draw as yourself.
  Future<void> _bookDraw() async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Book this draw'),
        content: const Text(
          'This asks finance to move the funded amount out of the scholarship '
          'fund into the order\'s fund, as one balanced transfer, carrying your '
          'own credential — finance\'s gate decides whether you may write. A '
          'draw is booked for a sale that happened, so the order must be paid or '
          'comped.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Book it'),
          ),
        ],
      ),
    );
    if (confirmed != true || !mounted) return;
    await _run(
      () => context.read<SessionState>().api.bookStoreDraw(widget.id),
      success: 'Draw booked.',
    );
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

  Future<void> _copy(String value) async {
    await Clipboard.setData(ClipboardData(text: value));
    if (!mounted) return;
    _say('Copied.');
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: Text('Order #${field(_order, ['id'], fallback: widget.id)}'),
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
                  // The server answers 403 with "no such order" for somebody
                  // else's order and for one that does not exist — deliberately
                  // the same answer, so this screen repeats it rather than
                  // guessing which.
                  message: 'Your own order needs store:read; anybody else\'s '
                      'needs store:read_all.'
                      '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
                )
              : _order.isEmpty
                  ? EmptyState(
                      icon: Icons.cloud_off,
                      title: 'Cannot reach the server',
                      message: _error ?? 'Nothing came back for this order.',
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
    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        Row(
          children: [
            Expanded(
              child: Text(
                'Order #${field(_order, ['id'])}',
                style: AppText.headlineMedium,
              ),
            ),
            StatusBadge(_status, label: storeStatusLabel(_status)),
          ],
        ),
        const SizedBox(height: AppSpacing.sm),
        _facts(),
        const SizedBox(height: AppSpacing.md),
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('What this order costs', style: AppText.titleLarge),
              const SizedBox(height: AppSpacing.md),
              StoreOrderMoney(order: _order),
            ],
          ),
        ),
        const SizedBox(height: AppSpacing.md),
        _linesCard(),
        const SizedBox(height: AppSpacing.md),
        StoreDrawSection(draw: _draw),
        const SizedBox(height: AppSpacing.md),
        _ledgerCard(),
        const SizedBox(height: AppSpacing.md),
        _payingCard(),
        if (_checkoutUrl.isNotEmpty) ...[
          const SizedBox(height: AppSpacing.md),
          _checkoutUrlCard(),
        ],
        const SizedBox(height: AppSpacing.md),
        _completionCard(scheme),
        const SizedBox(height: AppSpacing.lg),
        Text(
          'The shop holds no money and keeps no books: payment is stripe\'s and '
          'the ledger is finance\'s. What this screen shows is what both '
          'answered.',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
        const SizedBox(height: AppSpacing.lg),
      ],
    );
  }

  Widget _facts() {
    final compReason = field(_order, ['comp_reason']);
    final compBy = field(_order, ['comp_by']);
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          DetailField(label: 'Member', value: field(_order, ['member_id'])),
          DetailField(label: 'Placed by', value: field(_order, ['placed_by'])),
          DetailField(
            label: 'Price tier',
            value: field(_order, ['price_tier'], fallback: '—'),
          ),
          DetailField(
            label: 'Proceeds fund',
            value: field(_order, ['fund_code'], fallback: '—'),
          ),
          DetailField(
            label: 'Placed',
            value: formatDate(field(_order, ['created_at']), withTime: true),
          ),
          if (field(_order, ['completed_by']).isNotEmpty)
            DetailField(
              label: 'Completed by',
              value: '${field(_order, ['completed_by'])} · '
                  '${formatDate(field(_order, ['completed_at']), withTime: true)}',
            ),
          if (compReason.isNotEmpty)
            DetailField(
              label: 'Comp reason',
              value: compBy.isEmpty ? compReason : '$compReason — $compBy',
            ),
        ],
      ),
    );
  }

  /// The lines, with the shop's price and the charged unit kept apart on every
  /// one of them.
  Widget _linesCard() {
    final scheme = Theme.of(context).colorScheme;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Lines', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          if (_lines.isEmpty)
            Text(
              'The server returned no lines for this order.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            )
          else
            for (final line in _lines) _lineRow(line),
        ],
      ),
    );
  }

  Widget _lineRow(Map<String, dynamic> line) {
    final scheme = Theme.of(context).colorScheme;
    final quantity = int.tryParse(field(line, ['quantity'])) ?? 0;
    final listPrice = int.tryParse(field(line, ['list_price_cents']));
    final unitPrice = int.tryParse(field(line, ['unit_price_cents']));
    final total = int.tryParse(field(line, ['line_total_cents']));
    final rental = field(line, ['item_kind']).toLowerCase() == 'rental';
    final equipment = field(line, ['equipment_item_id']);

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
                  field(line, ['item_name'], fallback: 'Item'),
                  style: AppText.titleMedium,
                ),
              ),
              const SizedBox(width: AppSpacing.sm),
              Text(
                formatCents(total),
                style: AppText.titleMedium.copyWith(color: scheme.onSurface),
              ),
            ],
          ),
          const SizedBox(height: 2),
          Text(
            '$quantity × ${formatCents(unitPrice)} charged'
            '${listPrice != null && unitPrice != null && listPrice != unitPrice ? ' · the shop\'s price was ${formatCents(listPrice)} each' : ''}',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
          if (rental && equipment.isNotEmpty) ...[
            const SizedBox(height: 2),
            Text(
              'Rental of equipment item $equipment — this fee is the shop\'s; '
              'handing the item over is equipment\'s own checkout.',
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
        ],
      ),
    );
  }

  Widget _ledgerCard() {
    final scheme = Theme.of(context).colorScheme;
    final status = field(_ledger, ['status'], fallback: 'not_attempted');
    final transaction = field(_ledger, ['transaction_id']);
    final error = field(_ledger, ['error']);
    final paymentRef = field(_ledger, ['payment_ref']);
    final note = field(_ledger, ['note']);
    final confirmed = status == 'booked';
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(child: Text('The ledger', style: AppText.titleLarge)),
              StatusBadge(
                confirmed ? 'active' : 'review',
                label: switch (status) {
                  'not_attempted' => 'Not attempted',
                  'booked' => 'Booked',
                  'unbooked' => 'Not booked',
                  'refused' => 'Refused by finance',
                  'failed' => 'Failed',
                  _ => status,
                },
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          Text(
            confirmed
                ? 'Finance booked this order\'s payment'
                    '${transaction.isEmpty ? '' : ' as transaction $transaction'}.'
                : 'Finance has no confirmed entry for this order yet. The payment '
                    'may be real and unseen — a webhook confirms a payment to '
                    'stripe and carries no caller to reach this order — which is '
                    'why the order stays on the unsettled worklist.',
            style: AppText.bodyMedium,
          ),
          if (paymentRef.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Text('Stripe payment: $paymentRef', style: AppText.bodySmall),
          ],
          if (error.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Text(
              'Finance last answered: $error',
              style: AppText.bodySmall.copyWith(color: scheme.error),
            ),
          ],
          if (note.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(note, style: AppText.bodySmall.copyWith(color: scheme.outline)),
          ],
        ],
      ),
    );
  }

  /// Paying — offered when there is something to pay and the order is open.
  Widget _payingCard() {
    final scheme = Theme.of(context).colorScheme;
    final charged = _charged ?? 0;
    final open = _status == 'open';
    final awaiting = _status == 'awaiting_payment';

    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Paying', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          if (charged <= 0)
            Text(
              'This order charges nothing, so a Checkout session cannot take it '
              '— Stripe cannot take zero. At no charge the order is completed by '
              'authority instead: a comp, below, which records its reason and '
              'draws the whole price from the scholarship fund.',
              style: AppText.bodyMedium,
            )
          else if (open) ...[
            Text(
              'The Checkout session is opened for ${formatCents(charged)} — the '
              'amount charged, never the price, because the difference is the '
              'scholarship fund\'s business rather than the card\'s. The call '
              'carries your own credential and stripe\'s gate decides.',
              style: AppText.bodyMedium,
            ),
            const SizedBox(height: AppSpacing.md),
            if (_busy)
              const SizedBox(
                width: 24,
                height: 24,
                child: CircularProgressIndicator(strokeWidth: 2),
              )
            else
              FilledButton.icon(
                onPressed: _checkout,
                icon: const Icon(Icons.credit_card_outlined, size: 20),
                label: Text('Open a Checkout session for ${formatCents(charged)}'),
              ),
          ] else if (awaiting) ...[
            Text(
              'This order is awaiting payment'
              '${_checkoutUrl.isEmpty ? '' : ' and its Checkout session is below'}. '
              'Stripe confirms the payment to stripe, and this plugin cannot see '
              'that webhook, so a shopkeeper completes the order by hand once '
              'stripe\'s own record shows the payment.',
              style: AppText.bodyMedium,
            ),
            if (_checkoutUrl.isEmpty) ...[
              const SizedBox(height: AppSpacing.md),
              Text(
                'If no session was ever opened for it, the server\'s own record '
                'is still the truth: complete it below against the payment that '
                'paid it.',
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            ],
          ] else
            Text(
              'This order is ${storeStatusLabel(_status).toLowerCase()}: the '
              'charge has been settled and there is nothing left to pay.',
              style: AppText.bodyMedium,
            ),
        ],
      ),
    );
  }

  Widget _checkoutUrlCard() {
    final scheme = Theme.of(context).colorScheme;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              const Icon(Icons.open_in_new, size: 20),
              const SizedBox(width: AppSpacing.sm),
              Expanded(
                child: Text('Checkout session', style: AppText.titleLarge),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          SelectableText(_checkoutUrl, style: AppText.bodySmall),
          const SizedBox(height: AppSpacing.sm),
          Row(
            children: [
              TextButton.icon(
                onPressed: () => _copy(_checkoutUrl),
                icon: const Icon(Icons.copy, size: 18),
                label: const Text('Copy the link'),
              ),
            ],
          ),
          Text(
            'This build does not launch a browser for you, so the link is shown '
            'and copyable rather than dressed up as a button that cannot open '
            'it.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ],
      ),
    );
  }

  /// The acts that need an authority. Each names the permission it needs, and
  /// each is refused by the server rather than hidden by a client-side guess at
  /// who holds what.
  Widget _completionCard(ColorScheme scheme) {
    final paid = _status == 'paid';
    final comped = _status == 'comped';
    final open = _status == 'open' || _status == 'awaiting_payment';
    final funded = _funded ?? 0;

    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Completing and booking', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          Text(
            'These are authorities rather than prices, and each needs its own '
            'permission at troop scope: completing a paid order needs '
            'store:manage, comping one needs store:comp, and booking the draw '
            'needs store:manage plus finance:write of your own. Which roles hold '
            'them is the troop\'s business, recorded in the core — this screen '
            'offers the acts and lets the server answer.',
            style: AppText.bodyMedium.
                copyWith(color: scheme.onSurface.withValues(alpha: 0.8)),
          ),
          const SizedBox(height: AppSpacing.md),
          if (_busy)
            const SizedBox(
              width: 24,
              height: 24,
              child: CircularProgressIndicator(strokeWidth: 2),
            )
          else ...[
            if (open && (_charged ?? 0) > 0)
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.sm),
                child: OutlinedButton.icon(
                  onPressed: _complete,
                  icon: const Icon(Icons.verified_outlined, size: 20),
                  label: const Text('Complete against a stripe payment'),
                ),
              ),
            if (open)
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.sm),
                child: OutlinedButton.icon(
                  onPressed: _comp,
                  icon: const Icon(Icons.volunteer_activism_outlined, size: 20),
                  label: const Text('Comp this order (no charge)'),
                ),
              ),
            if ((paid || comped) && funded > 0 && !_drawBooked)
              OutlinedButton.icon(
                onPressed: _bookDraw,
                icon: const Icon(Icons.account_balance_outlined, size: 20),
                label: Text('Book the ${formatCents(funded)} draw'),
              ),
            if ((paid || comped) && funded > 0 && _drawBooked)
              Text(
                'The draw on this order is booked: there is nothing left to '
                'settle for it.',
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            if ((paid || comped) && funded <= 0)
              Text(
                'This order funds nothing, so there is no draw to book.',
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            if (!open && !paid && !comped)
              Text(
                'No completion act applies to an order in this state.',
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
          ],
        ],
      ),
    );
  }
}
