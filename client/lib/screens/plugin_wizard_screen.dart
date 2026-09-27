import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Choose which plugins this deployment runs.
///
/// The first-run flow issue #90 asks for, and the same decision the CLI makes
/// with `bootstrap-isolation --enable` — through the same server function, so the
/// two cannot land on different states.
///
/// Three things this screen deliberately does *not* decide for itself:
///
/// * **Which sets are legal.** The refusals come from the core, and this shows
///   the server's own words. A client-side copy of "auth is required" would be a
///   second source of truth for a rule the server enforces.
/// * **What turning something off costs.** The reason a plugin is required is a
///   statement about the server's behaviour; it arrives in the same payload as the
///   rule, so the sentence a leader reads and the rule the core applies cannot
///   disagree.
/// * **Whether the wizard should appear.** `choice == null` from the server — not
///   a flag on this device, which would re-prompt the second admin and could not
///   be set by a scripted install.
///
/// Nothing is applied until Save: a leader can look at the whole surface, and the
/// cost of each part of it, before anything changes. Nothing is deleted by turning
/// a plugin off, and the screen says so.
class PluginWizardScreen extends StatefulWidget {
  const PluginWizardScreen({super.key, this.firstRun = false});

  /// First run reads differently: it is a question, not an edit.
  final bool firstRun;

  @override
  State<PluginWizardScreen> createState() => _PluginWizardScreenState();
}

class _PluginWizardScreenState extends State<PluginWizardScreen> {
  List<Map<String, dynamic>> _plugins = const [];
  List<Map<String, dynamic>> _required = const [];
  List<Map<String, dynamic>> _dependencies = const [];
  Map<String, dynamic>? _recorded;

  final Set<String> _selected = {};

  bool _loading = true;
  bool _saving = false;
  String? _error;
  int? _errorStatus;
  bool _loadedOnce = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  bool get _refused => _errorStatus == 401 || _errorStatus == 403;

  Future<void> _load() async {
    final api = context.read<SessionState>().api;
    setState(() {
      _loading = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      final choice = await api.pluginChoice();
      final plugins = await api.plugins();

      final required = ((choice['required'] as List?) ?? const [])
          .whereType<Map>()
          .map((r) => r.cast<String, dynamic>())
          .toList();
      final dependencies = ((choice['dependencies'] as List?) ?? const [])
          .whereType<Map>()
          .map((d) => d.cast<String, dynamic>())
          .toList();
      final recorded = choice['choice'];
      final list = ((plugins['plugins'] as List?) ?? const [])
          .whereType<Map>()
          .map((p) => p.cast<String, dynamic>())
          .toList()
        ..sort((a, b) => field(a, ['id']).compareTo(field(b, ['id'])));

      if (!mounted) return;

      final selected = <String>{};
      if (recorded is Map) {
        // Editing an existing choice: start from what was chosen.
        for (final id in (recorded['plugin_ids'] as List?) ?? const []) {
          selected.add('$id');
        }
      } else {
        // First run starts minimal — only what cannot be off — and everything
        // else is one tap away. Starting from "all fourteen" is the one honest
        // option to avoid: it is a decision nobody made aloud.
        for (final r in required) {
          selected.add(field(r, ['id']));
        }
      }
      // A required plugin is on whatever else happens; the server refuses the
      // alternative, so the screen does not offer it.
      for (final r in required) {
        selected.add(field(r, ['id']));
      }

      setState(() {
        _plugins = list;
        _required = required;
        _dependencies = dependencies;
        _recorded = recorded is Map ? recorded.cast<String, dynamic>() : null;
        _selected
          ..clear()
          ..addAll(selected);
        _loading = false;
        _loadedOnce = true;
      });
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = e.message;
        _errorStatus = e.statusCode;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = e.toString();
      });
    }
  }

  Future<void> _save() async {
    final api = context.read<SessionState>().api;
    setState(() => _saving = true);
    try {
      final enabled = await api.setPluginChoice(_selected.toList());
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(
          content: Text('This deployment now runs ${enabled.length} plugin(s).'),
        ),
      );
      Navigator.of(context).pop(true);
    } on ApiException catch (e) {
      // The server's own words. It is the only thing that knows the rules, and a
      // refusal it chose to explain is more useful than anything this screen
      // could compose.
      if (!mounted) return;
      setState(() {
        _saving = false;
        _error = e.message;
        _errorStatus = e.statusCode;
      });
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text(e.message), backgroundColor: Theme.of(context).colorScheme.errorContainer),
      );
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _saving = false;
        _error = e.toString();
      });
    }
  }

  /// Why `id` cannot be turned off, in the server's words — or null.
  String? _requiredWhy(String id) {
    for (final r in _required) {
      if (field(r, ['id']) == id) return field(r, ['why']);
    }
    return null;
  }

  /// A dependency this selection does not satisfy, in the server's terms.
  ///
  /// Derived from the pairs the server reported rather than from a rule written
  /// here, so this can only ever warn about something the server would refuse.
  List<String> get _unsatisfied {
    final chosen = _selected;
    final out = <String>[];
    for (final d in _dependencies) {
      final dependent = field(d, ['dependent']);
      final dependency = field(d, ['dependency']);
      if (chosen.contains(dependent) && !chosen.contains(dependency)) {
        out.add('$dependent needs $dependency');
      }
    }
    return out;
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.firstRun ? 'What does this troop run?' : 'Plugins to run'),
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
                  title: 'Admins only',
                  message: 'Choosing what this deployment runs needs core:admin at '
                      'troop scope.'
                      '${(_error ?? '').isEmpty ? '' : '\n\nThe server said: $_error'}',
                )
              : (_error != null && !_loadedOnce)
                  ? EmptyState(
                      icon: Icons.cloud_off,
                      title: 'Cannot reach the server',
                      message: _error!,
                      action: FilledButton(
                        onPressed: _load,
                        child: const Text('Retry'),
                      ),
                    )
                  : _body(),
    );
  }

  Widget _body() {
    final unsatisfied = _unsatisfied;
    return Column(
      children: [
        Expanded(
          child: ListView(
            padding: const EdgeInsets.all(AppSpacing.md),
            children: [
              AppCard(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      widget.firstRun
                          ? 'Choose what this troop runs'
                          : 'What this deployment runs',
                      style: AppText.titleMedium,
                    ),
                    const SizedBox(height: AppSpacing.xs),
                    Text(
                      _recorded == null
                          // Said plainly, because "running everything" is a
                          // decision nobody made — and an operator should be able
                          // to tell that from one they did make.
                          ? 'No choice has been recorded, so this deployment is '
                              'running everything on disk.'
                          : 'Last chosen through the '
                              '${field(_recorded!, ['source'])} door'
                              '${field(_recorded!, ['chosen_by']).isEmpty ? ', with no authenticated user recorded' : ', by ${field(_recorded!, ['chosen_by'])}'}.',
                      style: AppText.bodySmall,
                    ),
                    const SizedBox(height: AppSpacing.xs),
                    Text(
                      'Nothing is deleted by leaving a plugin off, and this can be '
                      'changed later from Plugins. Everything outside the list below '
                      'is one tap away.',
                      style: AppText.bodySmall,
                    ),
                  ],
                ),
              ),
              const SizedBox(height: AppSpacing.md),
              ..._plugins.map(_row),
              const SizedBox(height: AppSpacing.md),
              Text(
                'Switching a plugin off stops it answering; it does not delete '
                'anything. Switching it back on re-binds its routes and its event '
                'subscriptions.',
                style: AppText.bodySmall,
              ),
            ],
          ),
        ),
        if (unsatisfied.isNotEmpty)
          Padding(
            padding: const EdgeInsets.fromLTRB(
              AppSpacing.md,
              0,
              AppSpacing.md,
              AppSpacing.sm,
            ),
            child: AppCard(
              child: Row(
                children: [
                  const Icon(Icons.link_off, size: 18),
                  const SizedBox(width: AppSpacing.sm),
                  Expanded(
                    child: Text(
                      '${unsatisfied.join('; ')}. The server will refuse this set.',
                      style: AppText.bodySmall,
                    ),
                  ),
                ],
              ),
            ),
          ),
        Padding(
          padding: const EdgeInsets.all(AppSpacing.md),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  '${_selected.length} of ${_plugins.length} on',
                  style: AppText.bodySmall,
                ),
              ),
              if (widget.firstRun)
                TextButton(
                  // Skipping is not "never asked": it records the minimal set,
                  // which is a decision stated rather than a default inherited.
                  onPressed: _saving ? null : _save,
                  child: const Text('Skip for now'),
                ),
              const SizedBox(width: AppSpacing.sm),
              FilledButton(
                onPressed: _saving ? null : _save,
                child: _saving
                    ? const SizedBox(
                        width: 18,
                        height: 18,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Text('Save'),
              ),
            ],
          ),
        ),
      ],
    );
  }

  Widget _row(Map<String, dynamic> plugin) {
    final id = field(plugin, ['id']);
    final name = field(plugin, ['name'], fallback: id);
    final why = _requiredWhy(id);
    final required = why != null;
    final on = _selected.contains(id);
    final routes = (plugin['routes'] as num?)?.toInt() ?? 0;
    final permissions = (plugin['permissions'] as List?)?.length ?? 0;

    return AppCard(
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Flexible(child: Text(name, style: AppText.titleMedium)),
                    const SizedBox(width: AppSpacing.sm),
                    if (required) const StatusBadge('required', label: 'Always on'),
                  ],
                ),
                const SizedBox(height: 2),
                Text(
                  '$routes route${routes == 1 ? '' : 's'} · '
                  '$permissions permission${permissions == 1 ? '' : 's'}',
                  style: AppText.bodySmall,
                ),
                if (required) ...[
                  const SizedBox(height: AppSpacing.xs),
                  Text(why, style: AppText.bodySmall),
                ],
              ],
            ),
          ),
          required
              ? const Padding(
                  padding: EdgeInsets.only(top: AppSpacing.sm),
                  child: Icon(Icons.lock_outline, size: 18),
                )
              : Switch(
                  value: on,
                  onChanged: _saving
                      ? null
                      : (value) => setState(() {
                            if (value) {
                              _selected.add(id);
                            } else {
                              _selected.remove(id);
                            }
                          }),
                ),
        ],
      ),
    );
  }
}
