import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';
import '../widgets/common.dart';

/// Plugins — what the core has loaded, and the switch for each.
///
/// The authority is the server's, not this screen's. Every call needs
/// `core:admin` at troop scope, and the "not for you" state is the server's
/// answer rather than a guess at which roles hold that permission — role →
/// permission lives in `core.role_permissions`, which only the core reads. So
/// the entry is always visible and the refusal is shown honestly, instead of the
/// app pretending to know who is entitled and being wrong in both directions.
///
/// Nothing here is cached, deliberately. A roster read is worth serving from
/// cache in the woods; this is live operational state, and an operator deciding
/// whether to switch a plugin off must be looking at what the server is running
/// *now*. A stale list would invite a decision about a plugin that has already
/// been reloaded or uninstalled underneath them.
class PluginsScreen extends StatefulWidget {
  const PluginsScreen({super.key});

  @override
  State<PluginsScreen> createState() => _PluginsScreenState();
}

class _PluginsScreenState extends State<PluginsScreen> {
  List<Map<String, dynamic>> _plugins = const [];
  List<String> _boundSubscriptions = const [];
  int _retiredLibraries = 0;
  bool _loading = true;
  String? _error;
  int? _errorStatus;

  /// The plugin id currently being switched, so only its own control spins.
  String? _busy;

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
      final data = await session.api.plugins();
      if (!mounted) return;
      setState(() {
        _plugins = ((data['plugins'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _boundSubscriptions =
            ((data['bound_subscriptions'] as List?) ?? const [])
                .map((e) => e.toString())
                .toList();
        _retiredLibraries = (data['retired_libraries'] as num?)?.toInt() ?? 0;
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

  Future<void> _toggle(Map<String, dynamic> plugin, bool enable) async {
    final id = field(plugin, ['id']);
    final name = field(plugin, ['name'], fallback: id);
    // Captured before the dialog: awaiting it is an async gap, and reaching for
    // the context on the other side of one is what the analyzer warns about.
    final session = context.read<SessionState>();
    if (!enable && !await _confirmDisable(name)) return;
    if (!mounted) return;

    setState(() => _busy = id);
    try {
      await session.api.setPluginEnabled(id, enable);
      if (!mounted) return;
      _say(enable ? '$name is enabled' : '$name is disabled');
      // Re-read rather than flipping the row locally: enable can change more
      // than the flag (subscriptions re-bind, schedules start), and the list
      // should show what the server now reports.
      await _load();
    } on ApiException catch (e) {
      if (!mounted) return;
      // The interesting refusal is 409 — the core will not let the last
      // identity provider go and says how to proceed. Show its words.
      _say(e.message, bad: true);
    } on Object catch (e) {
      if (!mounted) return;
      _say(e.toString(), bad: true);
    } finally {
      if (mounted) setState(() => _busy = null);
    }
  }

  Future<bool> _confirmDisable(String name) async {
    final answer = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Disable $name?'),
        content: const Text(
          'Its routes stop answering and its event subscriptions are aborted '
          'until it is enabled again. Nothing is deleted — the data and the '
          'schema stay where they are.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Keep it on'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Disable'),
          ),
        ],
      ),
    );
    return answer ?? false;
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
        title: const Text('Plugins'),
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
                  // State what is required, then keep the server's own words.
                  // A bare "forbidden" tells the reader nothing they can act on.
                  message: 'Managing plugins needs core:admin at troop scope.'
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
                  : _list(),
    );
  }

  Widget _list() {
    final on = _plugins.where((p) => p['enabled'] == true).length;
    final retired = _retiredLibraries > 0
        ? ' · $_retiredLibraries retired ${_retiredLibraries == 1 ? 'library' : 'libraries'}'
        : '';
    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        Text(
          '${_plugins.length} loaded · $on on, ${_plugins.length - on} off$retired',
          style: AppText.bodySmall,
        ),
        const SizedBox(height: AppSpacing.md),
        for (final plugin in _plugins) ...[
          _card(plugin),
          const SizedBox(height: AppSpacing.sm),
        ],
        const SizedBox(height: AppSpacing.sm),
        Text(
          'Switching a plugin off stops it answering; it does not delete '
          'anything. Switching it back on re-binds its routes and its event '
          'subscriptions.',
          style: AppText.bodySmall.copyWith(
            color: Theme.of(context).colorScheme.outline,
          ),
        ),
      ],
    );
  }

  Widget _card(Map<String, dynamic> plugin) {
    final id = field(plugin, ['id']);
    final name = field(plugin, ['name'], fallback: id);
    final version = field(plugin, ['version']);
    final kind = field(plugin, ['kind']);
    final on = plugin['enabled'] == true;
    final routes = (plugin['routes'] as num?)?.toInt() ?? 0;
    final permissions = (plugin['permissions'] as List?)?.length ?? 0;
    final schedules = (plugin['schedules'] as List?)?.length ?? 0;
    final isolated = plugin['isolated'] == true;
    final subscribed = _boundSubscriptions.contains(id);

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
                    StatusBadge(on ? 'active' : 'inactive', label: on ? 'On' : 'Off'),
                  ],
                ),
                const SizedBox(height: 2),
                Text('$id · v$version · $kind', style: AppText.bodySmall),
                const SizedBox(height: AppSpacing.sm),
                Wrap(
                  spacing: AppSpacing.md,
                  runSpacing: AppSpacing.xs,
                  children: [
                    _fact(Icons.alt_route, '$routes route${routes == 1 ? '' : 's'}'),
                    _fact(
                      Icons.key_outlined,
                      '$permissions permission${permissions == 1 ? '' : 's'}',
                    ),
                    if (schedules > 0)
                      _fact(
                        Icons.schedule,
                        '$schedules schedule${schedules == 1 ? '' : 's'}',
                      ),
                    if (subscribed) _fact(Icons.rss_feed, 'subscribed'),
                    if (isolated) _fact(Icons.shield_outlined, 'isolated'),
                  ],
                ),
              ],
            ),
          ),
          _busy == id
              ? const Padding(
                  padding: EdgeInsets.all(AppSpacing.md),
                  child: SizedBox(
                    width: 20,
                    height: 20,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  ),
                )
              : Switch(
                  value: on,
                  onChanged: (value) => _toggle(plugin, value),
                ),
        ],
      ),
    );
  }

  Widget _fact(IconData icon, String label) => Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(icon, size: 14, color: Theme.of(context).colorScheme.outline),
          const SizedBox(width: AppSpacing.xs),
          Text(label, style: AppText.bodySmall),
        ],
      );
}
