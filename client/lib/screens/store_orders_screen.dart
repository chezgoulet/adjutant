import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'store_order_screen.dart';

/// Orders — mine, or the troop's when the server says so.
///
/// Which orders appear here is **the server's answer, never this screen's
/// filter**: a caller without `store:read_all` is narrowed to their own orders
/// and the response says `narrowed_to_caller: true`, so the header can state the
/// truth instead of guessing at the reader's permissions.
///
/// Each row carries the three figures in miniature — price, charged, funded —
/// because the point of the shop's money model is that they differ, and a list
/// that showed one number would be the place that lie starts.
class StoreOrdersScreen extends StatefulWidget {
  const StoreOrdersScreen({super.key});

  @override
  State<StoreOrdersScreen> createState() => _StoreOrdersScreenState();
}

class _StoreOrdersScreenState extends State<StoreOrdersScreen> {
  List<Map<String, dynamic>> _orders = const [];
  bool _narrowed = true;
  String _note = '';
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

  Future<void> _load({bool silent = false}) async {
    final session = context.read<SessionState>();
    setState(() {
      if (!silent) _loading = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      final cached = await session.cachedMap(
        'store.orders',
        () => session.api.storeOrders(),
      );
      if (!mounted) return;
      final page = cached.value;
      setState(() {
        _orders = ((page['orders'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _narrowed = page['narrowed_to_caller'] != false;
        _note = field(page, ['note']);
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

  Future<void> _open(Map<String, dynamic> order) async {
    final id = field(order, ['id']);
    if (id.isEmpty) return;
    await Navigator.of(context).push(
      MaterialPageRoute<void>(builder: (_) => StoreOrderScreen(id: id)),
    );
    if (mounted) await _load(silent: true);
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Orders'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: _loading ? null : _load,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      body: Column(
        children: [
          if (_stale) OfflineBanner(cachedAt: _cachedAt),
          Expanded(
            child: _loading
                ? const Center(child: CircularProgressIndicator())
                : _refused
                    ? EmptyState(
                        icon: Icons.lock_outline,
                        title: 'Not yours to read',
                        message: 'Your own orders need store:read; anybody '
                            'else\'s needs store:read_all. The server refused, so '
                            'nothing is shown rather than a guess at what was '
                            'withheld.'
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
                        : _orders.isEmpty
                            ? const EmptyState(
                                icon: Icons.shopping_bag_outlined,
                                title: 'No orders yet',
                                message: 'Orders you place in the shop appear '
                                    'here with their price, what was charged and '
                                    'what the scholarship fund covered.',
                              )
                            : RefreshIndicator(
                                onRefresh: _load,
                                child: ListView.separated(
                                  padding: const EdgeInsets.all(AppSpacing.md),
                                  itemCount: _orders.length + 1,
                                  separatorBuilder: (_, _) =>
                                      const SizedBox(height: AppSpacing.sm),
                                  itemBuilder: (context, i) {
                                    if (i == _orders.length) {
                                      return _footer();
                                    }
                                    final order = _orders[i];
                                    return _OrderCard(
                                      order: order,
                                      showMember: !_narrowed,
                                      onTap: () => _open(order),
                                    );
                                  },
                                ),
                              ),
          ),
        ],
      ),
    );
  }

  Widget _footer() {
    final scheme = Theme.of(context).colorScheme;
    final scope = _narrowed
        ? 'These are your own orders: the server narrowed the list to you, '
            'because anybody else\'s needs store:read_all.'
        : 'This is the troop\'s orders: the server widened the list, so you hold '
            'store:read_all.';
    return Padding(
      padding: const EdgeInsets.only(top: AppSpacing.sm, bottom: AppSpacing.lg),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(scope, style: AppText.bodySmall.copyWith(color: scheme.outline)),
          if (_note.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Text(_note, style: AppText.bodySmall.copyWith(color: scheme.outline)),
          ],
        ],
      ),
    );
  }
}

class _OrderCard extends StatelessWidget {
  const _OrderCard({
    required this.order,
    required this.showMember,
    required this.onTap,
  });

  final Map<String, dynamic> order;
  final bool showMember;
  final VoidCallback onTap;

  int? get _price => int.tryParse(field(order, ['price_cents']));
  int? get _charged => int.tryParse(field(order, ['charged_cents']));
  int? get _funded => int.tryParse(field(order, ['funded_cents']));

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final status = field(order, ['status'], fallback: 'open').toLowerCase();
    final member = field(order, ['member_id']);
    final created = formatDate(field(order, ['created_at']), withTime: true);
    final tier = field(order, ['price_tier']);

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
          const SizedBox(height: 2),
          Text(
            [
              created,
              if (showMember && member.isNotEmpty) member,
              if (tier.isNotEmpty) '$tier tier',
            ].where((s) => s.isNotEmpty && s != '—').join(' · '),
            style: AppText.bodySmall,
          ),
          const SizedBox(height: AppSpacing.sm),
          // The three figures, named, in the order the money model states them.
          Wrap(
            spacing: AppSpacing.md,
            runSpacing: AppSpacing.xs,
            children: [
              _figure('Price', _price, scheme.onSurface),
              _figure('Charged', _charged, AppColors.primary),
              _figure(
                'Funded',
                _funded,
                (_funded ?? 0) > 0 ? AppColors.warning : AppColors.success,
              ),
            ],
          ),
          if ((_funded ?? 0) > 0) ...[
            const SizedBox(height: AppSpacing.xs),
            Text(
              'Funded by the scholarship fund — a draw, not a price of zero.',
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
        ],
      ),
    );
  }

  Widget _figure(String label, int? cents, Color emphasis) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(label, style: AppText.labelSmall.copyWith(color: emphasis)),
          Text(
            formatCents(cents),
            style: AppText.titleMedium.copyWith(color: emphasis),
          ),
        ],
      );
}

/// The order's own status words, as the shop states them. Never a wording this
/// client invented, and never a status the server cannot return.
String storeStatusLabel(String status) => switch (status) {
      'open' => 'Open',
      'awaiting_payment' => 'Awaiting payment',
      'paid' => 'Paid',
      'comped' => 'Comped',
      _ => status.isEmpty ? 'Unknown' : status,
    };
