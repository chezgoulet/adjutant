/// The shop's money, rendered honestly.
///
/// An order carries **three** figures and they are never the same number
/// (`plugins/store/src/lib.rs`, SPEC §7.16):
///
///  * `price_cents`   — what the goods cost at the shop's price, the sale's value
///  * `charged_cents` — what the member actually pays (`0` when comped)
///  * `funded_cents`  — `price_cents - charged_cents`, the draw on `scholarship`
///
/// Anything free, reduced or comped is a **draw on the scholarship fund** — a
/// real, positive transfer out of `scholarship` into the fund the order's
/// proceeds land in. So this file refuses the two dishonest renderings a shop
/// screen is tempted into: showing the charged amount as though it were the
/// price, and showing a funded amount as though it were free money from
/// nowhere. The three figures are labelled for what they are, and the sentence
/// beside them says which is which.
///
/// Money arrives in cents and is formatted by [formatCents] — the same single
/// convention the dues screen uses. There is no second money formatter here.
library;

import 'package:flutter/material.dart';

import '../theme/app_theme.dart';
import 'common.dart';

/// One of the three figures, with its own label and its own explanation.
class StoreMoneyRow extends StatelessWidget {
  const StoreMoneyRow({
    super.key,
    required this.label,
    required this.what,
    required this.cents,
    required this.emphasis,
  });

  /// What the figure is called on the order.
  final String label;

  /// One sentence saying what that figure means — so a reader never has to
  /// guess whether the number in front of them is the price or the charge.
  final String what;

  final int? cents;
  final Color emphasis;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.sm),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(label, style: AppText.bodyLarge),
                const SizedBox(height: 2),
                Text(
                  what,
                  style: AppText.bodySmall.copyWith(color: scheme.outline),
                ),
              ],
            ),
          ),
          const SizedBox(width: AppSpacing.sm),
          Text(
            formatCents(cents),
            style: AppText.titleLarge.copyWith(color: emphasis),
          ),
        ],
      ),
    );
  }
}

/// The order's three figures, side by side and named, with the one sentence
/// that makes the difference between them unmissable.
class StoreOrderMoney extends StatelessWidget {
  const StoreOrderMoney({super.key, required this.order});

  /// The order, as the server returns it. Every figure is read from the
  /// server's own field; none is computed here.
  final Map<String, dynamic> order;

  int? get _price => (order['price_cents'] as num?)?.toInt();
  int? get _charged => (order['charged_cents'] as num?)?.toInt();
  int? get _funded => (order['funded_cents'] as num?)?.toInt();

  /// The honest reading of the three numbers, in the shop's own terms.
  String get explanation {
    final funded = _funded ?? 0;
    final charged = _charged ?? 0;
    if (funded <= 0) {
      return charged == 0
          ? 'Nothing was funded and nothing was charged: the item\'s price is '
              'zero, so there is no subsidy to draw.'
          : 'Nothing was funded: the member paid the shop\'s whole price.';
    }
    if (charged == 0) {
      return 'The member was charged nothing: the whole price is a draw on the '
          'scholarship fund — a real transfer out of it, not a free order.';
    }
    return 'The member was charged ${formatCents(charged)} of the shop\'s '
        '${formatCents(_price)}. The remaining ${formatCents(funded)} is a draw '
        'on the scholarship fund — a subsidy, not a discount off the price and '
        'not money from nowhere.';
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        StoreMoneyRow(
          label: 'Price',
          what: 'what the goods cost at the shop\'s price — the sale\'s value',
          cents: _price,
          emphasis: scheme.onSurface,
        ),
        StoreMoneyRow(
          label: 'Charged',
          what: 'what the member actually pays',
          cents: _charged,
          emphasis: (_charged ?? 0) > 0 ? AppColors.primary : scheme.onSurface,
        ),
        StoreMoneyRow(
          label: 'Funded',
          what: 'the draw on the scholarship fund — price less what was charged',
          cents: _funded,
          emphasis: (_funded ?? 0) > 0 ? AppColors.warning : AppColors.success,
        ),
        const SizedBox(height: AppSpacing.xs),
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(Icons.info_outline, size: 16, color: scheme.outline),
            const SizedBox(width: AppSpacing.sm),
            Expanded(
              child: Text(
                explanation,
                style: AppText.bodySmall.copyWith(color: scheme.outline),
              ),
            ),
          ],
        ),
      ],
    );
  }
}

/// The draw as the server states it: how much, out of which fund into which,
/// and — the part that matters — how far it got.
///
/// A scholarship draw is a machine-originated money movement that may be
/// retried, so its state can be `unbooked` or `attempting` rather than `booked`.
/// This widget never renders a funded amount as settled unless the server said
/// `booked`.
class StoreDrawSection extends StatelessWidget {
  const StoreDrawSection({super.key, required this.draw});

  /// The order's `draw` block as the server returns it.
  final Map<String, dynamic> draw;

  String get _status => field(draw, ['status']).toLowerCase();

  bool get _booked => _status == 'booked';

  /// Human wording for a draw state, in the shop's own vocabulary.
  static String statusLabel(String status) => switch (status) {
        'none' => 'No draw',
        'unbooked' => 'Not booked yet',
        'attempting' => 'In flight',
        'booked' => 'Booked',
        'refused' => 'Refused by finance',
        'failed' => 'Failed',
        _ => status.isEmpty ? 'No draw' : status,
      };

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final funded = int.tryParse(field(draw, ['funded_cents']));
    final from = field(draw, ['from_fund_code']);
    final to = field(draw, ['to_fund_code']);
    final grouped = field(draw, ['transfer_group']);
    final error = field(draw, ['error']);
    final bookedAt = field(draw, ['booked_at']);
    final how = field(draw, ['how']);

    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text('Scholarship draw', style: AppText.titleLarge),
              ),
              StatusBadge(
                _booked ? 'active' : 'review',
                label: statusLabel(_status),
              ),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          if (funded == null || funded <= 0)
            Text(
              'This order funds nothing, so there is no draw on the '
              'scholarship fund to book.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            )
          else ...[
            Text(
              '${formatCents(funded)} out of the $from fund into the $to fund, '
              'as one balanced transfer — both legs, one statement, so the sum '
              'of every fund is unchanged.',
              style: AppText.bodyMedium,
            ),
            if (grouped.isNotEmpty) ...[
              const SizedBox(height: AppSpacing.xs),
              Text('Transfer group: $grouped', style: AppText.bodySmall),
            ],
            if (bookedAt.isNotEmpty) ...[
              const SizedBox(height: AppSpacing.xs),
              Text(
                'Booked ${formatDate(bookedAt, withTime: true)}',
                style: AppText.bodySmall,
              ),
            ],
            if (!_booked) ...[
              const SizedBox(height: AppSpacing.sm),
              Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const Icon(Icons.schedule, size: 16, color: AppColors.warning),
                  const SizedBox(width: AppSpacing.sm),
                  Expanded(
                    child: Text(
                      'This draw has not landed. A reduction applied by the shop '
                      'has no caller to forward a credential, so it is recorded '
                      'truthfully and left for a treasurer to book — it appears '
                      'on the unsettled worklist until it does.',
                      style: AppText.bodySmall.copyWith(color: AppColors.warning),
                    ),
                  ),
                ],
              ),
            ],
          ],
          if (error.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              'The draw last answered: $error',
              style: AppText.bodySmall.copyWith(color: scheme.error),
            ),
          ],
          if (how.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(how, style: AppText.bodySmall.copyWith(color: scheme.outline)),
          ],
        ],
      ),
    );
  }
}
