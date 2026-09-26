import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Governance — the troop's motions, where each one stands, and the caller's
/// own vote (SPEC §7.4, Accords Art 5/9/12/17).
///
/// The member-facing slice of the plugin, and nothing more: reading the motion
/// list, opening one motion's record (its tally, its quorum, and the caller's
/// own vote), and casting a vote through `POST /api/governance/motion/{id}/vote`.
/// Seconding, proposing, amendments, minutes, running a meeting and the Accords
/// archive belong to later slices and are deliberately absent — this screen
/// calls only the routes below and invents none.
///
/// The client counts nothing. Every tally, every quorum figure and the "would
/// this carry now" verdict come from the server (`GET /api/governance/motion/{id}`
/// returns the tally a close would produce at that moment), because a second
/// count of a vote is a second answer.
///
/// A refusal is not hidden: a `403` for a caller without `governance:read`, and
/// a `409` for a vote the caller may not cast, are shown in the server's own
/// words. A vote the caller may not cast — not a member, not present, already
/// voted, the motion closed — is answered with a `409`/`403`, and that refusal
/// *is* the answer.
class GovernanceScreen extends StatefulWidget {
  const GovernanceScreen({super.key});

  @override
  State<GovernanceScreen> createState() => _GovernanceScreenState();
}

class _GovernanceScreenState extends State<GovernanceScreen> {
  List<Map<String, dynamic>> _motions = const [];
  bool _loading = true;
  bool _stale = false;
  DateTime? _cachedAt;
  String? _error;
  int? _errorStatus;
  String _filter = 'all';

  /// The lifecycle's stages, as the server names them. The filter is a client
  /// convenience over one page the server already returned; it is not a second
  /// read and it guesses no stage name.
  static const _stages = [
    'all',
    'proposed',
    'seconded',
    'debate',
    'voting',
    'decided',
    'implemented',
    'withdrawn',
  ];

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
      final cached = await session.cachedList(
        'governance.motions',
        session.api.motions,
      );
      if (!mounted) return;
      setState(() {
        _motions = cached.value;
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

  List<Map<String, dynamic>> get _visible => _filter == 'all'
      ? _motions
      : _motions
          .where((m) => field(m, ['stage', 'status']).toLowerCase() == _filter)
          .toList();

  @override
  Widget build(BuildContext context) {
    // Its own Scaffold and title, because it is pushed from Settings rather
    // than being a shell destination — the way Dues is.
    return Scaffold(
      appBar: AppBar(title: const Text('Governance')),
      body: _body(),
    );
  }

  Widget _body() {
    if (_loading) return const Center(child: CircularProgressIndicator());

    final refused = _errorStatus == 401 || _errorStatus == 403;
    if (refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'The motions are not yours to read',
        message: 'Reading the troop\'s motions needs governance:read at troop '
            'scope.${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null && _motions.isEmpty) {
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
        SizedBox(
          height: 48,
          child: ListView(
            scrollDirection: Axis.horizontal,
            padding: const EdgeInsets.symmetric(horizontal: AppSpacing.md),
            children: [
              for (final stage in _stages)
                Padding(
                  padding: const EdgeInsets.only(right: AppSpacing.sm),
                  child: Center(
                    child: ChoiceChip(
                      label: Text(stage == 'all' ? 'All' : stageLabel(stage)),
                      selected: _filter == stage,
                      onSelected: (_) => setState(() => _filter = stage),
                    ),
                  ),
                ),
            ],
          ),
        ),
        Expanded(
          child: _visible.isEmpty
              ? const EmptyState(
                  icon: Icons.gavel_outlined,
                  title: 'No motions here',
                  message: 'A motion appears here once it is proposed. Its '
                      'stage is the server\'s word for where it stands — '
                      'proposed, seconded, in debate, voting, decided — and its '
                      'tally is the votes recorded so far.',
                )
              : RefreshIndicator(
                  onRefresh: _load,
                  child: ListView.separated(
                    padding: const EdgeInsets.all(AppSpacing.md),
                    itemCount: _visible.length,
                    separatorBuilder: (_, _) => const SizedBox(height: AppSpacing.sm),
                    itemBuilder: (context, i) => MotionCard(
                      motion: _visible[i],
                      onOpen: () => Navigator.of(context).push(
                        MaterialPageRoute<void>(
                          builder: (_) => MotionDetailScreen(
                            id: field(_visible[i], ['id']),
                            title: field(_visible[i], ['title'], fallback: 'Motion'),
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
        ),
      ],
    );
  }
}

/// A motion as one row: its title, its stage (and result, once decided), and
/// the tally the list itself carries.
class MotionCard extends StatelessWidget {
  const MotionCard({super.key, required this.motion, required this.onOpen});

  final Map<String, dynamic> motion;
  final VoidCallback onOpen;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final stage = field(motion, ['stage', 'status'], fallback: 'proposed');
    final result = field(motion, ['result']);
    final text = field(motion, ['text']);
    return AppCard(
      onTap: onOpen,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  field(motion, ['title'], fallback: 'Untitled motion'),
                  style: AppText.titleLarge,
                ),
              ),
              const SizedBox(width: AppSpacing.sm),
              StatusBadge(stage),
            ],
          ),
          if (result.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xs),
            Align(
              alignment: Alignment.centerLeft,
              child: StatusBadge(result),
            ),
          ],
          if (text.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.sm),
            Text(
              text,
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
              style: AppText.bodyMedium.copyWith(
                color: scheme.onSurface.withValues(alpha: 0.75),
              ),
            ),
          ],
          const SizedBox(height: AppSpacing.sm),
          Wrap(
            spacing: AppSpacing.md,
            runSpacing: 4,
            children: [
              TallyLine(motion: motion),
              if (field(motion, ['category']).isNotEmpty)
                Meta(Icons.label_outline, field(motion, ['category'])),
              if (field(motion, ['threshold']).isNotEmpty)
                Meta(Icons.percent_outlined,
                    thresholdLabel(field(motion, ['threshold']))),
            ],
          ),
        ],
      ),
    );
  }
}

/// The recorded tally as the list carries it: yes, no, abstain.
///
/// Named figures rather than a bar or a pie: a count that cannot be read as a
/// number is a count a scout cannot act on, and abstentions are shown because
/// they were recorded even though they decide nothing.
class TallyLine extends StatelessWidget {
  const TallyLine({super.key, required this.motion});

  final Map<String, dynamic> motion;

  @override
  Widget build(BuildContext context) {
    final yes = field(motion, ['votes_yes'], fallback: '0');
    final no = field(motion, ['votes_no'], fallback: '0');
    final abstain = field(motion, ['votes_abstain'], fallback: '0');
    if (yes == '0' && no == '0' && abstain == '0') {
      return Meta(Icons.how_to_vote_outlined, 'No votes recorded yet');
    }
    return Meta(Icons.how_to_vote_outlined,
        'Yes $yes · No $no · Abstain $abstain');
  }
}

/// One motion's record, with the caller's own vote and the one action this
/// slice offers: casting it.
class MotionDetailScreen extends StatefulWidget {
  const MotionDetailScreen({super.key, required this.id, this.title});

  final String id;
  final String? title;

  @override
  State<MotionDetailScreen> createState() => _MotionDetailScreenState();
}

class _MotionDetailScreenState extends State<MotionDetailScreen> {
  Map<String, dynamic> _page = const {};
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
      final cached = await session.cachedMap(
        'governance.motion.${widget.id}',
        () => session.api.motion(widget.id),
      );
      if (!mounted) return;
      setState(() {
        _page = cached.value;
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

  Map<String, dynamic> get _motion => _page['motion'] is Map
      ? Map<String, dynamic>.from(_page['motion'] as Map)
      : const {};

  Map<String, dynamic> get _tally => _page['tally'] is Map
      ? Map<String, dynamic>.from(_page['tally'] as Map)
      : const {};

  Map<String, dynamic>? get _quorum => _page['quorum'] is Map
      ? Map<String, dynamic>.from(_page['quorum'] as Map)
      : null;

  List<Map<String, dynamic>> get _votes => ((_page['votes'] as List?) ?? const [])
      .whereType<Map>()
      .map((e) => Map<String, dynamic>.from(e))
      .toList();

  /// The caller's own vote, told from the rest by its `voter` — the id
  /// `/api/auth/me` returned for this session. Compared as strings, because
  /// that is what both sides render to.
  Map<String, dynamic>? get _myVote {
    final me = field(context.read<SessionState>().user ?? const {}, ['id']);
    if (me.isEmpty) return null;
    for (final vote in _votes) {
      if (field(vote, ['voter']) == me) return vote;
    }
    return null;
  }

  /// A motion past its voteable stages: decided, implemented or withdrawn. The
  /// server refuses a vote then, and the screen offers none.
  bool get _closed {
    final stage = field(_motion, ['stage', 'status']).toLowerCase();
    return field(_motion, ['result']).isNotEmpty ||
        const ['decided', 'implemented', 'withdrawn'].contains(stage);
  }

  Future<void> _openVoteSheet() async {
    final recorded = await showModalBottomSheet<bool>(
      context: context,
      isScrollControlled: true,
      builder: (_) => VoteSheet(motionId: widget.id),
    );
    if (!mounted || recorded != true) return;
    // Re-read the server rather than editing the tally locally: the recorded
    // vote and the new numbers are the server's, not this screen's.
    await _load();
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      const SnackBar(content: Text('Your vote was recorded')),
    );
  }

  @override
  Widget build(BuildContext context) {
    final title = field(_motion, ['title'],
        fallback: (widget.title ?? '').isEmpty ? 'Motion' : widget.title!);
    return Scaffold(
      appBar: AppBar(title: Text(title)),
      body: _body(),
    );
  }

  Widget _body() {
    if (_loading) return const Center(child: CircularProgressIndicator());
    final refused = _errorStatus == 401 || _errorStatus == 403;
    if (refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'This motion is not yours to read',
        message: 'Reading one motion needs governance:read at troop scope.'
            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null) {
      return EmptyState(
        icon: _errorStatus == 404 ? Icons.search_off : Icons.cloud_off,
        title: _errorStatus == 404 ? 'No such motion' : 'Cannot reach the server',
        message: _error!,
        action: _errorStatus == 404
            ? null
            : FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }

    final motion = _motion;
    final tally = _tally;
    final quorum = _quorum;
    final myVote = _myVote;
    final stage = field(motion, ['stage', 'status'], fallback: 'proposed');
    final result = field(motion, ['result']);

    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Expanded(child: Text('Where it stands', style: AppText.titleMedium)),
                  StatusBadge(stage),
                ],
              ),
              if (result.isNotEmpty) ...[
                const SizedBox(height: AppSpacing.sm),
                Row(
                  children: [
                    Text('Outcome', style: AppText.bodyMedium),
                    const SizedBox(width: AppSpacing.sm),
                    StatusBadge(result),
                  ],
                ),
              ],
              const SizedBox(height: AppSpacing.md),
              if (field(motion, ['text']).isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(bottom: AppSpacing.md),
                  child: Text(field(motion, ['text']),
                      style: AppText.bodyMedium),
                ),
              for (final entry in <List<String>>[
                ['Body', bodyLabel(field(motion, ['body']))],
                ['Threshold', thresholdLabel(field(motion, ['threshold']))],
                ['Category', field(motion, ['category'])],
                ['Proposed by', field(motion, ['proposed_by'])],
                ['Seconded by', field(motion, ['seconded_by'])],
                ['Decided', formatDate(field(motion, ['decided_at']), withTime: true)],
              ])
                if (entry[1].isNotEmpty && entry[1] != '—')
                  DetailField(label: entry[0], value: entry[1]),
            ],
          ),
        ),
        const SizedBox(height: AppSpacing.md),
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('The tally', style: AppText.titleMedium),
              const SizedBox(height: AppSpacing.xs),
              Text(
                'The numbers a close would produce right now — counted by the '
                'server, not here.',
                style: AppText.bodySmall.copyWith(
                  color: Theme.of(context).colorScheme.outline,
                ),
              ),
              const SizedBox(height: AppSpacing.md),
              Row(
                children: [
                  Expanded(
                    child: _TallyCell('Yes', field(tally, ['yes'], fallback: '0')),
                  ),
                  Expanded(
                    child: _TallyCell('No', field(tally, ['no'], fallback: '0')),
                  ),
                  Expanded(
                    child:
                        _TallyCell('Abstain', field(tally, ['abstain'], fallback: '0')),
                  ),
                  Expanded(
                    child: _TallyCell('Cast', field(tally, ['cast'], fallback: '0')),
                  ),
                ],
              ),
              const SizedBox(height: AppSpacing.md),
              Text(
                tally['would_pass'] == true
                    ? 'This would carry now, on the '
                        '${thresholdLabel(field(tally, ['threshold'])).toLowerCase()}.'
                    : 'This would not carry as it stands, on the '
                        '${thresholdLabel(field(tally, ['threshold'])).toLowerCase()}.',
                style: AppText.bodyMedium,
              ),
            ],
          ),
        ),
        if (quorum != null) ...[
          const SizedBox(height: AppSpacing.md),
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(child: Text('Quorum', style: AppText.titleMedium)),
                    StatusBadge(quorum['met'] == true ? 'passed' : 'failed',
                        label: quorum['met'] == true ? 'Met' : 'Not met'),
                  ],
                ),
                const SizedBox(height: AppSpacing.sm),
                Text(
                  '${field(quorum, ['present'], fallback: '0')} present of '
                  '${field(quorum, ['required'], fallback: '0')} required — '
                  '${quorum['met'] == true ? 'the meeting can decide' : 'short by '
                      '${_shortBy(quorum)}'}.',
                  style: AppText.bodyMedium,
                ),
              ],
            ),
          ),
        ],
        const SizedBox(height: AppSpacing.md),
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text('Your vote', style: AppText.titleMedium),
              const SizedBox(height: AppSpacing.md),
              if (myVote == null)
                Text(
                  'You have not voted on this motion.',
                  style: AppText.bodyMedium,
                )
              else ...[
                Text(
                  'You voted ${choiceLabel(field(myVote, ['choice']))} '
                  '(${methodLabel(field(myVote, ['method']))}).',
                  style: AppText.bodyLarge,
                ),
                const SizedBox(height: AppSpacing.xs),
                Text(
                  'Recorded ${formatDate(field(myVote, ['recorded_at']), withTime: true)}',
                  style: AppText.bodySmall.copyWith(
                    color: Theme.of(context).colorScheme.outline,
                  ),
                ),
                if (field(myVote, ['note']).isNotEmpty) ...[
                  const SizedBox(height: AppSpacing.sm),
                  Text(field(myVote, ['note']), style: AppText.bodyMedium),
                ],
                const SizedBox(height: AppSpacing.sm),
                Text(
                  'One vote per member, and a recorded vote is not changed by '
                  'this app: a second vote is the server\'s refusal to give.',
                  style: AppText.bodySmall.copyWith(
                    color: Theme.of(context).colorScheme.outline,
                  ),
                ),
              ],
              if (!_closed) ...[
                const SizedBox(height: AppSpacing.md),
                FilledButton(
                  onPressed: _openVoteSheet,
                  child: const Text('Cast vote'),
                ),
              ],
            ],
          ),
        ),
      ],
    );
  }

  static String _shortBy(Map<String, dynamic> quorum) {
    final required = int.tryParse(field(quorum, ['required'], fallback: '0')) ?? 0;
    final present = int.tryParse(field(quorum, ['present'], fallback: '0')) ?? 0;
    final missing = required - present;
    return missing < 0 ? '0' : '$missing';
  }
}

class _TallyCell extends StatelessWidget {
  const _TallyCell(this.label, this.value);

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          label.toUpperCase(),
          style: AppText.labelSmall.copyWith(
            color: scheme.onSurface.withValues(alpha: 0.55),
            letterSpacing: 0.5,
          ),
        ),
        const SizedBox(height: 2),
        Text(value, style: AppText.titleLarge),
      ],
    );
  }
}

/// The vote sheet: a choice from the server's three, the method it was taken
/// by, and a note.
///
/// The body it posts is exactly the documented one — `{choice, method, note}` —
/// and the server's refusal, whatever it is, is shown in its own words while
/// the sheet stays open.
class VoteSheet extends StatefulWidget {
  const VoteSheet({super.key, required this.motionId});

  final String motionId;

  /// The three choices the route accepts, and the four methods.
  static const choices = ['yes', 'no', 'abstain'];
  static const methods = ['voice', 'show_of_hands', 'ballot', 'roll_call'];

  @override
  State<VoteSheet> createState() => _VoteSheetState();
}

class _VoteSheetState extends State<VoteSheet> {
  final _note = TextEditingController();
  String _choice = 'yes';
  String _method = 'voice';
  bool _saving = false;
  String? _error;
  int? _errorStatus;

  @override
  void dispose() {
    _note.dispose();
    super.dispose();
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  Future<void> _submit() async {
    final session = context.read<SessionState>();
    setState(() {
      _saving = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      await session.api.castVote(
        widget.motionId,
        choice: _choice,
        method: _method,
        note: _note.text.trim().isEmpty ? null : _note.text.trim(),
      );
      if (!mounted) return;
      Navigator.of(context).pop(true);
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
        _saving = false;
      });
    } on Object catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.toString();
        _saving = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.only(
        left: AppSpacing.md,
        right: AppSpacing.md,
        top: AppSpacing.md,
        bottom: MediaQuery.viewInsetsOf(context).bottom + AppSpacing.md,
      ),
      child: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Cast your vote', style: AppText.titleLarge),
            const SizedBox(height: AppSpacing.xs),
            Text(
              'One member, one vote. The server records it against this motion '
              'and the tally moves — a vote already recorded is not changed '
              'here, and the server says so if you try.',
              style: AppText.bodySmall.copyWith(
                color: Theme.of(context).colorScheme.outline,
              ),
            ),
            const SizedBox(height: AppSpacing.md),
            SegmentedButton<String>(
              segments: [
                for (final choice in VoteSheet.choices)
                  ButtonSegment<String>(
                    value: choice,
                    label: Text(choiceLabel(choice)),
                  ),
              ],
              selected: {_choice},
              onSelectionChanged: (s) => setState(() => _choice = s.first),
            ),
            const SizedBox(height: AppSpacing.md),
            DropdownButtonFormField<String>(
              initialValue: _method,
              decoration: const InputDecoration(
                labelText: 'Method',
                helperText: 'How the vote was taken — voice unless the room said otherwise',
              ),
              items: [
                for (final method in VoteSheet.methods)
                  DropdownMenuItem(value: method, child: Text(methodLabel(method))),
              ],
              onChanged: (v) => setState(() => _method = v ?? _method),
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: _note,
              decoration: const InputDecoration(labelText: 'Note'),
            ),
            if (_error != null) ...[
              const SizedBox(height: AppSpacing.md),
              RefusalMessage(error: _error!, refused: _refused),
            ],
            const SizedBox(height: AppSpacing.md),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                TextButton(
                  onPressed: _saving ? null : () => Navigator.of(context).pop(false),
                  child: const Text('Cancel'),
                ),
                const SizedBox(width: AppSpacing.sm),
                FilledButton(
                  onPressed: _saving ? null : _submit,
                  child: Text(_saving ? 'Casting…' : 'Cast my vote'),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

/// A refusal from an action: the server's own message, with the permission
/// named only when the refusal was a permission one.
class RefusalMessage extends StatelessWidget {
  const RefusalMessage({super.key, required this.error, required this.refused});

  final String error;
  final bool refused;

  @override
  Widget build(BuildContext context) {
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(AppSpacing.sm),
      decoration: BoxDecoration(
        color: Theme.of(context).colorScheme.errorContainer,
        borderRadius: BorderRadius.circular(AppRadius.sm),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            refused
                ? 'The server refused — voting needs governance:vote at troop '
                    'scope, and a member of the meeting it belongs to.'
                : 'The server refused',
            style: AppText.labelLarge.copyWith(
              color: Theme.of(context).colorScheme.onErrorContainer,
            ),
          ),
          const SizedBox(height: 4),
          Text(
            error,
            style: AppText.bodyMedium.copyWith(
              color: Theme.of(context).colorScheme.onErrorContainer,
            ),
          ),
        ],
      ),
    );
  }
}

/// A small icon-and-text fact, as the list rows use it.
class Meta extends StatelessWidget {
  const Meta(this.icon, this.text, {super.key});

  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 14, color: scheme.outline),
        const SizedBox(width: 4),
        Text(text, style: AppText.bodySmall),
      ],
    );
  }
}

/// The server's stage strings, in a scout's words.
String stageLabel(String stage) => switch (stage.toLowerCase()) {
      'proposed' => 'Proposed',
      'seconded' => 'Seconded',
      'debate' => 'In Debate',
      'voting' => 'Voting',
      'decided' => 'Decided',
      'implemented' => 'Implemented',
      'withdrawn' => 'Withdrawn',
      'passed' => 'Passed',
      'failed' => 'Failed',
      _ => stage,
    };

/// The three thresholds the SPEC names.
String thresholdLabel(String threshold) => switch (threshold) {
      'simple_majority' => 'Simple majority',
      'two_thirds' => 'Two thirds',
      'unanimous' => 'Unanimous',
      _ => threshold.isEmpty ? 'Simple majority' : threshold,
    };

/// The four vote methods the route accepts.
String methodLabel(String method) => switch (method) {
      'voice' => 'Voice',
      'show_of_hands' => 'Show of hands',
      'ballot' => 'Ballot',
      'roll_call' => 'Roll call',
      _ => method.isEmpty ? 'Voice' : method,
    };

/// The three choices the route accepts.
String choiceLabel(String choice) => switch (choice.toLowerCase()) {
      'yes' => 'Yes',
      'no' => 'No',
      'abstain' => 'Abstain',
      _ => choice,
    };

/// The four bodies a motion can be raised in (the server's vocabulary).
String bodyLabel(String body) => switch (body.toLowerCase()) {
      'congress' => 'Congress',
      'tc' => 'Troop Council',
      'lodge' => 'Lodge',
      'committee' => 'Committee',
      _ => body,
    };
