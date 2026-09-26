import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
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
///  * **What is covered** is the row's own `funded_cents` — what the Scholarship
///    fund pays of the assessment. A waiver is not a price of zero: the
///    assessment stands and all of it is funded, so the member's own share is
///    zero and the screen says **covered**. A self-reported reduction funds the
///    discount. The draw that moves the money (`draw_status`, `draw_ref`) is the
///    troop's business, not the scout's, and never appears here.
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
    // What scholarship covers of the assessment. A waiver funds the whole of it,
    // so the member's own share is zero; a self-reported reduction funds the
    // discount. The figure is the server's, taken off the row.
    final funded = int.tryParse(field(dues, ['funded_cents']));
    final covered = (funded ?? 0) > 0;
    final outstandingCents = outstanding ?? 0;
    final settled = dues['settled'] == true;
    // What a covered member reads. The draw that funds it — its state, its
    // reference, the fund it draws on — is the troop's business and is not shown
    // here: a scout's screen says their dues are covered, not who booked what.
    final coverageNote = !covered
        ? ''
        : outstandingCents <= 0
            ? 'Covered — your dues for this year are paid for you. Nothing is '
                'owed.'
            : 'Covered — the covered part of your dues is paid for you. What '
                'remains is what you owe above.';

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
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('My dues', style: AppText.titleLarge),
                  const SizedBox(height: AppSpacing.sm),
                  // The headline is the one figure a scout came for: what is
                  // still owed, as the ledger derived it. Everything below it
                  // explains how the server got there.
                  _OwedHeadline(cents: outstanding),
                  const Divider(height: AppSpacing.lg),
                  _MoneyRow(
                    label: 'Assessed',
                    cents: assessed,
                    emphasis: scheme.onSurface,
                  ),
                  if (covered) ...[
                    const SizedBox(height: AppSpacing.sm),
                    _MoneyRow(
                      label: 'Covered',
                      cents: funded,
                      emphasis: AppColors.success,
                    ),
                  ],
                  const SizedBox(height: AppSpacing.sm),
                  _MoneyRow(
                    label: 'Paid',
                    cents: paid,
                    emphasis: AppColors.success,
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
                  if (coverageNote.isNotEmpty) ...[
                    const SizedBox(height: AppSpacing.sm),
                    Text(
                      coverageNote,
                      style: AppText.bodySmall.copyWith(color: scheme.outline),
                    ),
                  ],
                ],
              ),
            ),
          const SizedBox(height: AppSpacing.md),
          _tierCard(dues),
          const SizedBox(height: AppSpacing.md),
          _paymentsCard(),
          const SizedBox(height: AppSpacing.md),
          DuesPaymentSection(
            member: _member,
            duesYear: _fiscalYear,
            outstandingCents: outstandingCents,
            onRefresh: () => _load(silent: true),
          ),
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
        // A waiver is not a price of zero: the assessment stands and the whole of
        // it is *funded*, so the member's own share is zero. The scout reads that
        // their dues are **covered** — the draw that funds it, and the fund it
        // draws on, are the troop's business, not the scout's, and do not appear
        // here.
        'waived' => 'Covered',
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

/// The one figure a scout opens this screen for: what is still owed, taken
/// straight from the standing the ledger derived (`outstanding_cents`) and never
/// re-added here. Zero reads green; anything left reads as the warning it is.
class _OwedHeadline extends StatelessWidget {
  const _OwedHeadline({required this.cents});

  final int? cents;

  @override
  Widget build(BuildContext context) {
    final owed = cents ?? 0;
    return Row(
      crossAxisAlignment: CrossAxisAlignment.end,
      children: [
        Expanded(child: Text('You owe', style: AppText.bodyLarge)),
        Text(
          formatCents(cents),
          style: AppText.displayMedium.copyWith(
            color: owed > 0 ? AppColors.warning : AppColors.success,
          ),
        ),
      ],
    );
  }
}

/// Paying dues — a member may pay their own dues from here.
///
/// `POST /api/stripe/checkout` (`stripe:checkout`, any scope) opens a Stripe
/// Checkout session for what is owed: `{purpose: "dues", amount_cents, dues_year,
/// member_id}`, the member naming only themselves. Opening it charges nothing —
/// Stripe confirms the payment, the webhook books the income in finance under
/// category `dues` against that member, and only then does the standing move. So
/// the session is opened, its URL is shown to be copied (this build adds no
/// browser dependency, and invents none), and the payment's landing is read back
/// rather than assumed.
///
/// Two rules the money holds this screen to:
///  * **Pay is offered only when something is owed.** Stripe cannot take zero
///    and the server refuses a non-positive amount, so a covered or settled
///    member sees why there is nothing to pay rather than a button that cannot
///    work.
///  * **A `503` is stated as itself**: the troop's Stripe is unconfigured, and
///    the screen says so rather than showing a broken button.
///
/// The treasurer's own route (`POST /api/finance/dues/payment`, `finance:write`)
/// is a *different* act with a different authority and is not this screen's: a
/// member pays through Stripe, and the ledger entry follows from the payment.
class DuesPaymentSection extends StatefulWidget {
  const DuesPaymentSection({
    super.key,
    required this.member,
    required this.duesYear,
    required this.outstandingCents,
    required this.onRefresh,
  });

  /// The caller's own roster id — the only member this screen may pay for.
  final String member;

  /// The fiscal year the standing is for; omitted from the request when unknown,
  /// so the server's own default applies.
  final int? duesYear;

  /// What the standing says is still owed, as `outstanding_cents`.
  final int outstandingCents;

  /// Re-read the standing — the ledger, not this screen, decides what is owed.
  final VoidCallback onRefresh;

  @override
  State<DuesPaymentSection> createState() => _DuesPaymentSectionState();
}

class _DuesPaymentSectionState extends State<DuesPaymentSection> {
  String _checkoutUrl = '';
  List<Map<String, dynamic>> _sessions = const [];
  bool _busy = false;
  String? _error;
  int? _errorStatus;

  @override
  void initState() {
    super.initState();
    _loadSessions();
  }

  Future<void> _loadSessions() async {
    final session = context.read<SessionState>();
    try {
      final sessions = await session.api.duesSessions(memberId: widget.member);
      if (!mounted) return;
      setState(() => _sessions = sessions);
    } on ApiException {
      // The payment status is a courtesy; a refusal here must not take the pay
      // affordance down with it — what is owed is the more important fact.
    } on Object {
      // Offline, most likely. Same reasoning.
    }
  }

  Future<void> _pay() async {
    if (widget.outstandingCents <= 0) return;
    setState(() {
      _busy = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      final response = await context.read<SessionState>().api.payDues(
            amountCents: widget.outstandingCents,
            duesYear: widget.duesYear,
            memberId: widget.member,
          );
      if (!mounted) return;
      final url = field(response, ['checkout_url']);
      setState(() => _checkoutUrl = url);
      if (url.isEmpty) {
        _say('The server opened a session but returned no link to it.', bad: true);
      }
      // The payment has not landed until it lands: re-read the sessions and the
      // standing rather than claiming a state the ledger has not reached.
      await _loadSessions();
      widget.onRefresh();
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
      });
      _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      _say('Cannot reach the server — no session was opened.', bad: true);
      debugPrint('dues checkout failed: $e');
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

  Future<void> _copy(String value) async {
    await Clipboard.setData(ClipboardData(text: value));
    if (!mounted) return;
    _say('Copied.');
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final owed = widget.outstandingCents;
    final unconfigured = _errorStatus == 503;

    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Paying dues', style: AppText.titleLarge),
          const SizedBox(height: AppSpacing.sm),
          if (owed <= 0)
            Text(
              'There is nothing left to pay, so this screen offers no payment — '
              'a Checkout session charges, and Stripe cannot take zero. The '
              'standing above is the ledger\'s own answer.',
              style: AppText.bodyMedium.copyWith(color: scheme.outline),
            )
          else ...[
            Text(
              'Open a Stripe Checkout session for ${formatCents(owed)} — what you '
              'owe, and nothing more. The session charges nothing by itself: once '
              'Stripe confirms your payment, finance books it against your name '
              'and the figure above moves. This build shows the link to open '
              'rather than launching a browser for you.',
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
                onPressed: _pay,
                icon: const Icon(Icons.credit_card_outlined, size: 20),
                label: Text('Pay ${formatCents(owed)} with Stripe'),
              ),
            if (unconfigured) ...[
              const SizedBox(height: AppSpacing.sm),
              Text(
                'This troop\'s Stripe is not configured, so no payment can be '
                'taken from the app yet. The server said: $_error',
                style: AppText.bodySmall.copyWith(color: scheme.error),
              ),
            ] else if (_error != null) ...[
              const SizedBox(height: AppSpacing.sm),
              Text(
                'The server refused the session: $_error',
                style: AppText.bodySmall.copyWith(color: scheme.error),
              ),
            ],
          ],
          if (_checkoutUrl.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.md),
            _checkoutUrlCard(scheme),
          ],
          if (_sessions.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.md),
            _sessionsCard(scheme),
          ],
        ],
      ),
    );
  }

  Widget _checkoutUrlCard(ColorScheme scheme) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            const Icon(Icons.open_in_new, size: 20),
            const SizedBox(width: AppSpacing.sm),
            Expanded(child: Text('Checkout session', style: AppText.titleMedium)),
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
          'and copyable rather than dressed up as a button that cannot open it.',
          style: AppText.bodySmall.copyWith(color: scheme.outline),
        ),
      ],
    );
  }

  Widget _sessionsCard(ColorScheme scheme) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text('Your payment sessions', style: AppText.titleMedium),
        const SizedBox(height: AppSpacing.sm),
        for (final s in _sessions)
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
                        _sessionStatusLabel(field(s, ['status'])),
                        style: AppText.bodyMedium,
                      ),
                      const SizedBox(height: 2),
                      Text(
                        formatDate(field(s, ['created_at']), withTime: true),
                        style: AppText.bodySmall.copyWith(color: scheme.outline),
                      ),
                    ],
                  ),
                ),
                const SizedBox(width: AppSpacing.sm),
                Text(
                  formatCents(int.tryParse(field(s, ['amount_cents']))),
                  style: AppText.titleMedium,
                ),
              ],
            ),
          ),
        Row(
          children: [
            TextButton.icon(
              onPressed: _loadSessions,
              icon: const Icon(Icons.refresh, size: 18),
              label: const Text('Check again'),
            ),
          ],
        ),
      ],
    );
  }

  /// The session's state, in a member's words: has the payment landed or not.
  static String _sessionStatusLabel(String status) => switch (status) {
        'pending' => 'Opening…',
        'created' => 'Awaiting payment',
        'completed' => 'Stripe confirmed the payment',
        'expired' => 'Expired — not paid',
        'failed' => 'Failed — not paid',
        _ => status,
      };
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
