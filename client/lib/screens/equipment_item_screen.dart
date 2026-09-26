import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';
import 'equipment_screen.dart';

/// One piece of gear: what it is, whether it is out and to whom, and the two
/// actions a scout performs here — taking it out and bringing it back.
///
/// The screen fetches `GET /api/equipment/item/{id}` (the item, its open
/// checkout, the last twenty checkouts and the derived flags) and offers the
/// actions the routes gate:
///
///  * **Check out** — `POST …/checkout`, with the due date, purpose, the mission
///    it is for (chosen from the troop's own missions, never a free-text id),
///    the destination and the grade it leaves in.
///  * **Check in** — `POST …/checkin`, stating the grade it came back in.
///
/// A refusal is not hidden: a `409` for an item that is retired, in
/// maintenance, unserviceable or already out is shown in the server's own
/// words, because that refusal is the answer to the question the scout asked.
class EquipmentItemScreen extends StatefulWidget {
  const EquipmentItemScreen({super.key, required this.id});

  final String id;

  @override
  State<EquipmentItemScreen> createState() => _EquipmentItemScreenState();
}

class _EquipmentItemScreenState extends State<EquipmentItemScreen> {
  Map<String, dynamic> _page = const {};
  List<Map<String, dynamic>> _missions = const [];
  bool _loading = true;
  String? _error;
  int? _errorStatus;

  @override
  void initState() {
    super.initState();
    _load();
    _loadMissions();
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
        'equipment.item.${widget.id}',
        () => session.api.equipmentItem(widget.id),
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

  /// The missions the app already knows, so `mission_id` is a choice rather
  /// than a free-text id. A failure here is not fatal: a checkout without a
  /// mission is allowed, and the sheet says the field is left out.
  Future<void> _loadMissions() async {
    try {
      final missions = await context.read<SessionState>().api.missions();
      if (!mounted) return;
      setState(() => _missions = missions);
    } on Object {
      // No missions known; the checkout sheet simply omits the field.
    }
  }

  Map<String, dynamic> get _item =>
      _page['item'] is Map ? Map<String, dynamic>.from(_page['item'] as Map) : const {};

  Map<String, dynamic>? get _openCheckout =>
      _page['open_checkout'] is Map
          ? Map<String, dynamic>.from(_page['open_checkout'] as Map)
          : null;

  @override
  Widget build(BuildContext context) {
    final name = field(_item, ['name'], fallback: 'Equipment');
    return Scaffold(
      appBar: AppBar(title: Text(name)),
      body: _body(),
    );
  }

  Widget _body() {
    if (_loading) return const Center(child: CircularProgressIndicator());
    final refused = _errorStatus == 401 || _errorStatus == 403;
    if (refused) {
      return EmptyState(
        icon: Icons.lock_outline,
        title: 'This item is not yours to read',
        message: 'Reading one item needs equipment:read at troop scope.'
            '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
      );
    }
    if (_error != null) {
      return EmptyState(
        icon: _errorStatus == 404 ? Icons.search_off : Icons.cloud_off,
        title: _errorStatus == 404 ? 'No such item' : 'Cannot reach the server',
        message: _error!,
        action: _errorStatus == 404
            ? null
            : FilledButton(onPressed: _load, child: const Text('Retry')),
      );
    }

    final item = _item;
    final open = _openCheckout;
    final flags = _page['flags'] is Map
        ? Map<String, dynamic>.from(_page['flags'] as Map)
        : const <String, dynamic>{};
    final history = ((_page['checkouts'] as List?) ?? const [])
        .whereType<Map>()
        .map((e) => Map<String, dynamic>.from(e))
        .toList();

    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        AppCard(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(item['name']?.toString() ?? 'Unnamed item', style: AppText.titleLarge),
              const SizedBox(height: AppSpacing.sm),
              Wrap(
                spacing: AppSpacing.sm,
                runSpacing: AppSpacing.xs,
                children: [
                  StatusBadge(
                    field(item, ['status'], fallback: 'available'),
                    label: itemStatusLabel(field(item, ['status'], fallback: 'available')),
                  ),
                  StatusBadge(
                    'active',
                    label: 'Condition: ${conditionLabel(field(item, ['condition'], fallback: 'good'))}',
                  ),
                ],
              ),
              const SizedBox(height: AppSpacing.md),
              for (final entry in <List<String>>[
                ['Asset tag', field(item, ['asset_tag'])],
                ['Category', field(item, ['category'])],
                ['Location', field(item, ['location'])],
                ['Acquired', formatDate(field(item, ['acquired_on']))],
                ['Next service', formatDate(field(item, ['next_service_on']))],
                ['Description', field(item, ['description'])],
              ])
                if (entry[1].isNotEmpty) DetailField(label: entry[0], value: entry[1]),
            ],
          ),
        ),
        const SizedBox(height: AppSpacing.md),
        _outCard(open),
        const SizedBox(height: AppSpacing.md),
        if (_flagReasons(flags).isNotEmpty) ...[
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Flags', style: AppText.titleMedium),
                const SizedBox(height: AppSpacing.sm),
                for (final reason in _flagReasons(flags))
                  Padding(
                    padding: const EdgeInsets.only(bottom: 4),
                    child: Text('• ${_flagLabel(reason)}', style: AppText.bodyMedium),
                  ),
              ],
            ),
          ),
          const SizedBox(height: AppSpacing.md),
        ],
        if (history.isNotEmpty) ...[
          AppCard(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Recent checkouts', style: AppText.titleMedium),
                const SizedBox(height: AppSpacing.sm),
                for (final row in history.take(5))
                  Padding(
                    padding: const EdgeInsets.only(bottom: AppSpacing.sm),
                    child: Text(
                      _historyLine(row),
                      style: AppText.bodySmall,
                    ),
                  ),
              ],
            ),
          ),
        ],
      ],
    );
  }

  Widget _outCard(Map<String, dynamic>? open) {
    final scheme = Theme.of(context).colorScheme;
    if (open == null) {
      return AppCard(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('In the pool', style: AppText.titleMedium),
            const SizedBox(height: AppSpacing.xs),
            Text(
              'Nobody has this item out. Checking it out is the server\'s '
              'decision — it answers with its own reason when the item may not '
              'leave.',
              style: AppText.bodySmall.copyWith(color: scheme.outline),
            ),
            const SizedBox(height: AppSpacing.md),
            FilledButton.icon(
              onPressed: _checkout,
              icon: const Icon(Icons.outbound_outlined),
              label: const Text('Check out'),
            ),
          ],
        ),
      );
    }
    final due = field(open, ['due_on']);
    return AppCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(child: Text('Checked out', style: AppText.titleMedium)),
              const StatusBadge('pending', label: 'Out'),
            ],
          ),
          const SizedBox(height: AppSpacing.sm),
          DetailField(label: 'Held by', value: field(open, ['checked_out_by'], fallback: 'unknown')),
          DetailField(label: 'Out since', value: formatDate(field(open, ['checked_out_on']))),
          DetailField(
            label: 'Due back',
            value: due.isEmpty ? 'No due date' : formatDate(due),
          ),
          if (field(open, ['purpose']).isNotEmpty)
            DetailField(label: 'Purpose', value: field(open, ['purpose'])),
          if (field(open, ['mission_id']).isNotEmpty)
            DetailField(label: 'Mission', value: '#${field(open, ['mission_id'])}'),
          const SizedBox(height: AppSpacing.sm),
          FilledButton.icon(
            onPressed: _checkin,
            icon: const Icon(Icons.move_to_inbox_outlined),
            label: const Text('Check in'),
          ),
        ],
      ),
    );
  }

  List<String> _flagReasons(Map<String, dynamic> flags) {
    final reasons = <String>[];
    final replacement = flags['replacement'];
    if (replacement is Map) {
      final list = (replacement['reasons'] as List?) ?? const [];
      reasons.addAll(list.map((e) => e.toString()));
    }
    return reasons;
  }

  String _flagLabel(String reason) {
    if (reason == 'flagged') return 'Flagged for replacement by hand';
    if (reason == 'service_count') return 'Service count past the troop\'s threshold';
    if (reason == 'age') return 'Older than the troop\'s replacement age';
    if (reason.startsWith('condition_')) {
      return 'Condition ${reason.substring('condition_'.length)}';
    }
    return reason;
  }

  String _historyLine(Map<String, dynamic> row) {
    final who = field(row, ['checked_out_by'], fallback: 'somebody');
    final outOn = formatDate(field(row, ['checked_out_on']));
    final inOn = field(row, ['checked_in_on']);
    final open = row['open'] == true;
    return open
        ? '$who took it on $outOn — still out'
        : '$who took it on $outOn, back ${formatDate(inOn)} '
            '(${conditionLabel(field(row, ['condition_in']))})';
  }

  Future<void> _checkout() async {
    final itemId = field(_item, ['id'], fallback: widget.id);
    final done = await showModalBottomSheet<bool>(
      context: context,
      isScrollControlled: true,
      builder: (_) => _CheckoutSheet(
        itemId: itemId,
        itemCondition: field(_item, ['condition'], fallback: 'good'),
        missions: _missions,
      ),
    );
    if (done == true && mounted) {
      await _load();
    }
  }

  Future<void> _checkin() async {
    final itemId = field(_item, ['id'], fallback: widget.id);
    final done = await showModalBottomSheet<bool>(
      context: context,
      isScrollControlled: true,
      builder: (_) => _CheckinSheet(
        itemId: itemId,
        itemCondition: field(_item, ['condition'], fallback: 'good'),
      ),
    );
    if (done == true && mounted) {
      await _load();
    }
  }
}

// ---------------------------------------------------------------------------
// Check out
// ---------------------------------------------------------------------------

class _CheckoutSheet extends StatefulWidget {
  const _CheckoutSheet({
    required this.itemId,
    required this.itemCondition,
    required this.missions,
  });

  final String itemId;
  final String itemCondition;
  final List<Map<String, dynamic>> missions;

  @override
  State<_CheckoutSheet> createState() => _CheckoutSheetState();
}

class _CheckoutSheetState extends State<_CheckoutSheet> {
  final _purpose = TextEditingController();
  final _destination = TextEditingController();
  final _note = TextEditingController();
  late String _condition = widget.itemCondition;
  DateTime? _dueOn;
  int? _missionId;
  bool _saving = false;
  String? _error;
  int? _errorStatus;

  @override
  void dispose() {
    _purpose.dispose();
    _destination.dispose();
    _note.dispose();
    super.dispose();
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  Future<void> _pickDue() async {
    final picked = await showDatePicker(
      context: context,
      initialDate: _dueOn ?? DateTime.now(),
      firstDate: DateTime.now().subtract(const Duration(days: 1)),
      lastDate: DateTime(2100),
    );
    if (picked != null && mounted) setState(() => _dueOn = picked);
  }

  Future<void> _submit() async {
    final session = context.read<SessionState>();
    setState(() {
      _saving = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      await session.api.checkoutEquipmentItem(
        widget.itemId,
        dueOn: _dueOn == null ? null : isoDate(_dueOn!),
        purpose: _purpose.text.trim().isEmpty ? null : _purpose.text.trim(),
        missionId: _missionId,
        destination:
            _destination.text.trim().isEmpty ? null : _destination.text.trim(),
        condition: _condition,
        note: _note.text.trim().isEmpty ? null : _note.text.trim(),
      );
      if (!mounted) return;
      Navigator.of(context).pop(true);
    } on ApiException catch (e) {
      if (!mounted) return;
      // The refusal is the answer: the server's own words, kept, not replaced.
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
            Text('Check out', style: AppText.titleLarge),
            const SizedBox(height: AppSpacing.xs),
            Text(
              'The holder defaults to you — the route takes the caller. A due '
              'date is a promise: leave it off and an open checkout blocks the '
              'item until you bring it back.',
              style: AppText.bodySmall.copyWith(
                color: Theme.of(context).colorScheme.outline,
              ),
            ),
            const SizedBox(height: AppSpacing.md),
            Row(
              children: [
                Expanded(
                  child: OutlinedButton.icon(
                    onPressed: _pickDue,
                    icon: const Icon(Icons.event_outlined, size: 18),
                    label: Text(
                      _dueOn == null ? 'No due date' : 'Due ${formatDate(isoDate(_dueOn!))}',
                    ),
                  ),
                ),
                if (_dueOn != null)
                  IconButton(
                    tooltip: 'Clear due date',
                    onPressed: () => setState(() => _dueOn = null),
                    icon: const Icon(Icons.clear),
                  ),
              ],
            ),
            const SizedBox(height: AppSpacing.md),
            _MissionField(
              missions: widget.missions,
              value: _missionId,
              onChanged: (v) => setState(() => _missionId = v),
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: _purpose,
              decoration: const InputDecoration(
                labelText: 'Purpose',
                helperText: 'What it is for — a hunt, a service, a camp',
              ),
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: _destination,
              decoration: const InputDecoration(labelText: 'Destination'),
            ),
            const SizedBox(height: AppSpacing.md),
            DropdownButtonFormField<String>(
              initialValue: _condition,
              decoration: const InputDecoration(
                labelText: 'Condition out',
                helperText: 'The grade the item leaves in',
              ),
              items: [
                for (final grade in kConditionGrades)
                  DropdownMenuItem(value: grade, child: Text(conditionLabel(grade))),
              ],
              onChanged: (v) => setState(() => _condition = v ?? _condition),
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: _note,
              decoration: const InputDecoration(labelText: 'Note'),
            ),
            if (_error != null) ...[
              const SizedBox(height: AppSpacing.md),
              _Refusal(error: _error!, refused: _refused),
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
                  child: Text(_saving ? 'Checking out…' : 'Check it out'),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

/// The mission chooser: a dropdown over the missions the app already knows, or
/// an explicit note that the field is left out because none are known. Never a
/// free-text id.
class _MissionField extends StatelessWidget {
  const _MissionField({
    required this.missions,
    required this.value,
    required this.onChanged,
  });

  final List<Map<String, dynamic>> missions;
  final int? value;
  final ValueChanged<int?> onChanged;

  @override
  Widget build(BuildContext context) {
    if (missions.isEmpty) {
      return Text(
        'No mission is known to this app, so the checkout is sent without a '
        'mission. It is still a valid checkout — a mission only links it to the '
        'work it is for.',
        style: AppText.bodySmall.copyWith(
          color: Theme.of(context).colorScheme.outline,
        ),
      );
    }
    final ids = <int>{
      for (final m in missions)
        if (int.tryParse(field(m, ['id'])) != null) int.parse(field(m, ['id'])),
    };
    return DropdownButtonFormField<int?>(
      initialValue: ids.contains(value) ? value : null,
      decoration: const InputDecoration(
        labelText: 'Mission',
        helperText: 'Which mission this gear is for — chosen from the app\'s missions',
      ),
      items: [
        const DropdownMenuItem<int?>(value: null, child: Text('No mission')),
        for (final id in ids.toList()..sort())
          DropdownMenuItem<int?>(
            value: id,
            child: Text(_missionLabel(id)),
          ),
      ],
      onChanged: onChanged,
    );
  }

  String _missionLabel(int id) {
    for (final m in missions) {
      if (field(m, ['id']) == '$id') {
        final title = field(m, ['title'], fallback: 'Mission');
        return '#$id — $title';
      }
    }
    return '#$id';
  }
}

// ---------------------------------------------------------------------------
// Check in
// ---------------------------------------------------------------------------

class _CheckinSheet extends StatefulWidget {
  const _CheckinSheet({required this.itemId, required this.itemCondition});

  final String itemId;
  final String itemCondition;

  @override
  State<_CheckinSheet> createState() => _CheckinSheetState();
}

class _CheckinSheetState extends State<_CheckinSheet> {
  final _note = TextEditingController();
  late String _condition = widget.itemCondition;
  bool _damaged = false;
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
      await session.api.checkinEquipmentItem(
        widget.itemId,
        condition: _condition,
        note: _note.text.trim().isEmpty ? null : _note.text.trim(),
        damaged: _damaged,
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
            Text('Check in', style: AppText.titleLarge),
            const SizedBox(height: AppSpacing.xs),
            Text(
              'State the grade it came back in — a checkin that does not name a '
              'condition cannot attribute damage to a period of use. It closes '
              'the open checkout and carries the grade onto the item.',
              style: AppText.bodySmall.copyWith(
                color: Theme.of(context).colorScheme.outline,
              ),
            ),
            const SizedBox(height: AppSpacing.md),
            DropdownButtonFormField<String>(
              initialValue: _condition,
              decoration: const InputDecoration(
                labelText: 'Condition in',
                helperText: 'Required — the grade it is now',
              ),
              items: [
                for (final grade in kConditionGrades)
                  DropdownMenuItem(value: grade, child: Text(conditionLabel(grade))),
              ],
              onChanged: (v) => setState(() => _condition = v ?? _condition),
            ),
            const SizedBox(height: AppSpacing.md),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Damaged'),
              subtitle: const Text(
                'Left off, the server decides from the two grades it recorded.',
              ),
              value: _damaged,
              onChanged: (v) => setState(() => _damaged = v),
            ),
            const SizedBox(height: AppSpacing.sm),
            TextField(
              controller: _note,
              decoration: const InputDecoration(labelText: 'Note'),
            ),
            if (_error != null) ...[
              const SizedBox(height: AppSpacing.md),
              _Refusal(error: _error!, refused: _refused),
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
                  child: Text(_saving ? 'Checking in…' : 'Check it in'),
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
class _Refusal extends StatelessWidget {
  const _Refusal({required this.error, required this.refused});

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
                ? 'The server refused — checking gear out needs equipment:checkout '
                    'at troop scope.'
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
