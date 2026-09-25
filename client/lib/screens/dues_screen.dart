import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Dues — what I owe, and reporting my own sliding-scale tier.
///
/// Two facts, kept apart on purpose:
///
///  * **What I owe** is derived by the server from the ledger
///    (`assessed_cents`, `paid_cents`, `outstanding_cents`, `settled`). This
///    screen renders those figures; it never adds money up, because a second
///    implementation of that is a second answer.
///  * **What tier I am on** is mine to say. The sliding scale is honor-system
///    (Accords): nobody verifies income, `hardship` assesses $0 so cost never
///    decides who belongs, and a self-report never sets the base cost it is a
///    fraction of — that is the treasurer's number, and the server says so with
///    a 409 rather than inventing a price.
///
/// The tier chooser is built from the server's own scale (`GET
/// /api/finance/sliding-scale` — labels, shares, assessed amounts and the
/// sentence a scout reads when choosing), so no amount appears here that the
/// server did not state. If that read is refused, the chooser says so instead of
/// offering options the caller may not have.
///
/// Reads go through [SessionState.cachedMap]: what you owe is exactly the thing
/// worth having in the woods with no signal, and [OfflineBanner] says which hour
/// the cached answer is from.
class DuesScreen extends StatefulWidget {
  const DuesScreen({super.key});

  @override
  State<DuesScreen> createState() => _DuesScreenState();
}

class _DuesScreenState extends State<DuesScreen> {
  /// The standing row plus what the ledger says about it (null until the
  /// treasurer has opened an assessment, or until one is known).
  Map<String, dynamic>? _dues;
  List<Map<String, dynamic>> _payments = const [];
  int? _fiscalYear;

  /// The sliding scale, as the server states it.
  Map<String, dynamic> _scale = const {};
  String? _scaleError;

  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;
  bool _busy = false;

  /// The caller's own member id. This is what the dues route takes, and it is
  /// the one record the caller may read with `finance:read` alone.
  String _member = '';

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load({bool silent = false}) async {
    final session = context.read<SessionState>();
    _member = (session.user?['id'] as String?)?.trim() ?? '';
    setState(() {
      if (!silent) _loading = true;
      _error = null;
      _errorStatus = null;
    });
    if (_member.isEmpty) {
      setState(() {
        _loading = false;
        _error = 'This session has no member id, so there is no dues record to '
            'read. Signing in again will fix it.';
      });
      return;
    }
    try {
      final standing = await session.cachedMap(
        'dues.$_member',
        () => session.api.memberDues(_member),
      );
      if (!mounted) return;
      final data = standing.value;
      setState(() {
        _dues = (data['dues'] as Map?)?.cast<String, dynamic>();
        _payments = ((data['payments'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _fiscalYear = (data['fiscal_year'] as num?)?.toInt();
        _stale = standing.isStale;
        _cachedAt = standing.cachedAt;
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
    // The scale is a second, independent read. A refusal there must not take
    // the standing down with it — what you owe is the more important fact.
    await _loadScale();
  }

  Future<void> _loadScale() async {
    final session = context.read<SessionState>();
    try {
      final scale = await session.cachedMap('dues.scale', session.api.slidingScale);
      if (!mounted) return;
      setState(() {
        _scale = scale.value;
        _scaleError = null;
      });
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() => _scaleError = e.message);
    } on Object catch (e) {
      if (!mounted) return;
      setState(() => _scaleError = 'Cannot reach the server');
      debugPrint('sliding scale unavailable: $e');
    }
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  /// Offline with nothing cached: say so, rather than rendering an empty record
  /// as though the troop had assessed nothing.
  bool get _nothingKnown =>
      _stale && _dues == null && _payments.isEmpty && _fiscalYear == null;

  List<Map<String, dynamic>> get _tiers =>
      ((_scale['tiers'] as List?) ?? const [])
          .whereType<Map>()
          .map((e) => Map<String, dynamic>.from(e))
          .toList();

  Future<void> _reportTier(String tier) async {
    final session = context.read<SessionState>();
    setState(() => _busy = true);
    try {
      final response = await session.api.selfReportDues(tier: tier);
      if (!mounted) return;
      final assessed = field(response, ['assessed_display']);
      _say(
        assessed.isEmpty
            ? 'Tier reported to the treasurer.'
            : 'Tier reported — assessed $assessed.',
      );
      // Re-read rather than patching the row locally: the standing is the
      // ledger's, and the ledger has just been written to.
      await _load(silent: true);
    } on ApiException catch (e) {
      if (!mounted) return;
      // The interesting refusal is 409 — no assessment and no configured
      // membership cost, so there is no base to be a fraction of. The server
      // says exactly that, and it is the message the scout needs.
      _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      _say('Cannot reach the server — nothing was reported.', bad: true);
      debugPrint('self-report failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _chooseTier() async {
    final current = field(_dues ?? const {}, ['tier']);
    final chosen = await showModalBottomSheet<String>(
      context: context,
      isScrollControlled: true,
      backgroundColor: Theme.of(context).colorScheme.surface,
      builder: (context) => _TierSheet(
        tiers: _tiers,
        current: current,
        honorNote: field(_scale, ['note']),
        baseDisplay: field(_scale, ['base_display']),
        baseConfigured: _scale['base_configured'] == true,
      ),
    );
    if (chosen == null || !mounted) return;
    await _reportTier(chosen);
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
        title: const Text('Dues'),
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
                        message:
                            'A member\'s own dues need finance:read at any scope; '
                            'anybody else\'s needs finance:read_all. The server '
                            'refused, so nothing is shown here rather than a guess '
                            'at what was withheld.'
                            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
                      )
                    : _error != null || _nothingKnown
                        ? EmptyState(
                            icon: Icons.cloud_off,
                            title: _nothingKnown
                                ? 'Offline — nothing cached yet'
                                : 'Cannot reach the server',
                            message: _error ??
                                'This record has not been read successfully yet, '
                                    'so there is no last-known answer to show.',
                            action: FilledButton(
                              onPressed: _load,
                              child: const Text('Retry'),
                            ),
                          )
                        : _content(),
          ),
        ],
      ),
    );
  }

  Widget _content() {
    final scheme = Theme.of(context).colorScheme;
    final dues = _dues ?? const <String, dynamic>{};
    final status = field(dues, ['status']);
    final assessed = int.tryParse(field(dues, ['assessed_cents']));
    final paid = int.tryParse(field(dues, ['paid_cents']));
    final outstanding = int.tryParse(field(dues, ['outstanding_cents']));
    final settled = dues['settled'] == true;

    return RefreshIndicator(
      onRefresh: _load,
      child: ListView(
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  _fiscalYear == null ? 'Dues' : 'Dues ${_fiscalYear!}',
                  style: AppText.headlineMedium,
                ),
              ),
              if (status.isNotEmpty)
                StatusBadge(status, label: _statusLabel(status)),
            ],
          ),
          const SizedBox(height: AppSpacing.md),
          if (dues.isEmpty)
            AppCard(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('No assessment open yet', style: AppText.titleLarge),
                  const SizedBox(height: AppSpacing.sm),
                  Text(
                    'The treasurer opens an assessment, or the troop configures '
                    'its membership cost — a self-report never sets the base cost '
                    'it is a fraction of. Report your tier below and the server '
                    'will tell you whether it can be assessed yet.',
                    style: AppText.bodyMedium,
                  ),
                ],
              ),
            )
          else
            AppCard(
              child: Column(
                children: [
                  _MoneyRow(
                    label: 'Assessed',
                    cents: assessed,
                    emphasis: scheme.onSurface,
                  ),
                  const Divider(height: AppSpacing.lg),
                  _MoneyRow(
                    label: 'Paid',
                    cents: paid,
                    emphasis: AppColors.success,
                  ),
                  const Divider(height: AppSpacing.lg),
                  _MoneyRow(
                    label: 'Outstanding',
                    cents: outstanding,
                    emphasis:
                        (outstanding ?? 0) > 0 ? AppColors.warning : AppColors.success,
                  ),
                  const SizedBox(height: AppSpacing.sm),
                  Row(
                    children: [
                      Icon(
                        settled ? Icons.check_circle_outline : Icons.schedule,
                        size: 16,
                        color: settled ? AppColors.success : scheme.outline,
                      ),
                      const SizedBox(width: AppSpacing.sm),
                      Expanded(
                        child: Text(
                          settled
                              ? 'Settled — nothing outstanding for the year'
                              : 'Not settled — the ledger shows a balance still to come',
                          style: AppText.bodySmall,
                        ),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          const SizedBox(height: AppSpacing.md),
          _tierCard(dues),
          const SizedBox(height: AppSpacing.md),
          _paymentsCard(),
          const SizedBox(height: AppSpacing.md),
          const DuesPaymentSection(),
          const SizedBox(height: AppSpacing.md),
          Text(
            _scaleError != null
                ? 'The sliding scale could not be read: $_scaleError'
                : 'The sliding scale is honor-system: no income verification is '
                    'asked for or recorded, hardship assesses \$0, and nobody is '
                    'turned away for it.',
            style: AppText.bodySmall.copyWith(color: scheme.outline),
          ),
        ],
      ),
    );
  }

  Widget _tierCard(Map<String, dynamic> dues) {
    final scheme = Theme.of(context).colorScheme;
    final tier = field(dues, ['tier']);
    final selfReported = dues['self_reported'] == true;
    final base = int.tryParse(field(dues, ['base_cents']));
    final note = field(dues, ['note']);

    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('My tier', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          if (tier.isEmpty)
            Text(
              'No tier on record for this year yet.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            )
          else
            Text(
              '${_labelFor(tier)}'
              '${selfReported ? ' — self-reported' : ''}',
              style: AppText.titleMedium,
            ),
          const SizedBox(height: AppSpacing.xs),
          if (base != null)
            Text(
              'Assessed as a share of the troop\'s membership cost '
              '(${formatCents(base)}).',
              style: AppText.bodySmall,
            ),
          if (note.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Text('Note on record: $note', style: AppText.bodySmall),
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
              onPressed: _tiers.isEmpty ? null : _chooseTier,
              icon: const Icon(Icons.tune, size: 20),
              label: Text(tier.isEmpty ? 'Report my tier' : 'Change my tier'),
            ),
          if (_tiers.isEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              'The tiers come from the server: without the sliding scale there '
                  'are no options to offer, and this client will not invent '
                  'amounts for them.',
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
        ],
      ),
    );
  }

  Widget _paymentsCard() {
    final scheme = Theme.of(context).colorScheme;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Payments', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          if (_payments.isEmpty)
            Text(
              'No dues payments recorded against you this year. A payment is an '
              'income entry the treasurer records; your standing above moves the '
              'moment it is booked.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            )
          else
            for (final p in _payments)
              Padding(
                padding: const EdgeInsets.only(bottom: AppSpacing.sm),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            field(p, ['description'], fallback: 'Dues payment'),
                            style: AppText.titleMedium,
                          ),
                          const SizedBox(height: 2),
                          Text(
                            [
                              formatDate(field(p, ['occurred_on'])),
                              field(p, ['kind']),
                            ].where((s) => s.isNotEmpty && s != '—').join(' · '),
                            style: AppText.bodySmall,
                          ),
                        ],
                      ),
                    ),
                    const SizedBox(width: AppSpacing.sm),
                    Text(
                      formatCents(int.tryParse(field(p, ['amount_cents']))),
                      style: AppText.titleMedium.copyWith(
                        color: AppColors.success,
                      ),
                    ),
                  ],
                ),
              ),
        ],
      ),
    );
  }

  static String _statusLabel(String status) => switch (status) {
        'self_reported' => 'Self-reported',
        'assessed' => 'Assessed',
        'waived' => 'Waived',
        _ => status,
      };

  /// The scale's own label when the server has stated it, else the code as the
  /// server spelled it — never a wording this client made up.
  String _labelFor(String tier) {
    for (final t in _tiers) {
      if (field(t, ['tier']) == tier) {
        final label = field(t, ['label']);
        if (label.isNotEmpty) return label;
      }
    }
    return tier;
  }
}

class _MoneyRow extends StatelessWidget {
  const _MoneyRow({
    required this.label,
    required this.cents,
    required this.emphasis,
  });

  final String label;
  final int? cents;
  final Color emphasis;

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        Expanded(child: Text(label, style: AppText.bodyLarge)),
        Text(
          formatCents(cents),
          style: AppText.titleLarge.copyWith(color: emphasis),
        ),
      ],
    );
  }
}

/// The place a payment affordance goes.
///
/// Paying is deliberately not built here: it belongs to the payments plugin
/// (Stripe, SPEC §7.13), which books an income entry and lets the ledger move
/// the standing this screen already renders. Rather than a button that does
/// nothing — or one that pretends to — this section states where it will live,
/// so the affordance drops in here and nothing above it has to move.
class DuesPaymentSection extends StatelessWidget {
  const DuesPaymentSection({super.key});

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Paying', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          Text(
            'Not available yet. Paying dues from the app lands with the payments '
            'plugin; until it does, a treasurer records the payment and your '
            'standing above updates from the ledger. No card details are asked '
            'for here, because nothing here can take them.',
            style: AppText.bodyMedium.copyWith(color: scheme.outline),
          ),
        ],
      ),
    );
  }
}

/// The tier chooser — the scale, as the server states it.
///
/// A bottom sheet, per the design language: forms and selections on a phone are
/// a sheet, and every row here is a 56dp target for a scout with cold hands.
class _TierSheet extends StatelessWidget {
  const _TierSheet({
    required this.tiers,
    required this.current,
    required this.honorNote,
    required this.baseDisplay,
    required this.baseConfigured,
  });

  final List<Map<String, dynamic>> tiers;
  final String current;
  final String honorNote;
  final String baseDisplay;
  final bool baseConfigured;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return SafeArea(
      child: ListView(
        shrinkWrap: true,
        padding: const EdgeInsets.all(AppSpacing.md),
        children: [
          Text('The sliding scale', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          Text(
            baseConfigured
                ? 'The troop\'s membership cost is $baseDisplay, and each tier is '
                    'a share of it. Choose the one that is true.'
                : 'The troop has not configured a membership cost, so the scale '
                    'shows zeroes until it does.',
            style: AppText.bodyMedium,
          ),
          if (honorNote.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              honorNote,
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
          ],
          const SizedBox(height: AppSpacing.md),
          for (final t in tiers)
            Padding(
              padding: const EdgeInsets.only(bottom: AppSpacing.sm),
              child: AppCard(
                onTap: () => Navigator.of(context).pop(field(t, ['tier'])),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Row(
                            children: [
                              Flexible(
                                child: Text(
                                  field(t, ['label'], fallback: field(t, ['tier'])),
                                  style: AppText.titleMedium,
                                ),
                              ),
                              if (field(t, ['tier']) == current) ...[
                                const SizedBox(width: AppSpacing.sm),
                                const StatusBadge('active', label: 'Current'),
                              ],
                            ],
                          ),
                          const SizedBox(height: AppSpacing.xs),
                          Text(
                            field(t, ['description']),
                            style: AppText.bodySmall,
                          ),
                        ],
                      ),
                    ),
                    const SizedBox(width: AppSpacing.md),
                    Text(
                      field(t, ['assessed_display'], fallback: '—'),
                      style: AppText.titleLarge,
                    ),
                  ],
                ),
              ),
            ),
          const SizedBox(height: AppSpacing.sm),
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Cancel'),
          ),
        ],
      ),
    );
  }
}
