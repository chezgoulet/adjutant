import 'dart:io';

import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'package:provider/provider.dart';
import 'package:share_plus/share_plus.dart';

import '../api/api_client.dart';
import '../state/session.dart';
import '../theme/app_theme.dart';

/// Backups — the cadence, the button, and the bundles.
///
/// The authority is the server's, as it is on the plugins screen: every call
/// needs `core:backup` at troop scope, and a refusal is shown as the server's
/// answer rather than a guess at which roles hold the grant. A troop can hand
/// someone backups without making them an admin, so the app must not assume the
/// two travel together.
///
/// Nothing here is cached. A stale list would invite an admin to download a
/// bundle that has since been pruned, or to believe a schedule is set when the
/// server has already been told otherwise.
class BackupsScreen extends StatefulWidget {
  const BackupsScreen({super.key});

  @override
  State<BackupsScreen> createState() => _BackupsScreenState();
}

class _BackupsScreenState extends State<BackupsScreen> {
  List<Map<String, dynamic>> _presets = const [];
  List<Map<String, dynamic>> _runs = const [];
  Map<String, dynamic> _schedule = const {};
  String _directory = '';

  int? _cadence;
  int _keep = 14;
  bool _enabled = false;

  bool _loading = true;
  bool _saving = false;
  bool _running = false;
  String? _error;
  int? _errorStatus;

  /// The bundle currently being fetched, so only its own row spins.
  String? _fetching;

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
      final data = await session.api.backups();
      if (!mounted) return;
      setState(() {
        _presets = ((data['presets'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _runs = ((data['runs'] as List?) ?? const [])
            .whereType<Map>()
            .map((e) => Map<String, dynamic>.from(e))
            .toList();
        _schedule =
            Map<String, dynamic>.from((data['schedule'] as Map?) ?? const {});
        _directory = (data['directory'] as String?) ?? '';
        _cadence = (_schedule['cadence_secs'] as num?)?.toInt();
        _keep = (_schedule['keep'] as num?)?.toInt() ?? 14;
        _enabled = _schedule['enabled'] == true;
        _loading = false;
      });
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = e.message;
        _errorStatus = e.statusCode;
      });
    } on OfflineException {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = 'Cannot reach the server.';
      });
    }
  }

  Future<void> _save() async {
    final session = context.read<SessionState>();
    setState(() {
      _saving = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      await session.api.setBackupSchedule(
        // Deliberately null rather than omitted when the switch is off: the route
        // reads absent as "leave the cadence alone" and null as "no schedule".
        cadenceSecs: _enabled ? _cadence : null,
        keep: _keep,
        enabled: _enabled,
      );
      if (!mounted) return;
      await _load();
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
      });
    } on OfflineException {
      if (!mounted) return;
      setState(() => _error = 'Cannot reach the server.');
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  Future<void> _runNow() async {
    final session = context.read<SessionState>();
    setState(() {
      _running = true;
      _error = null;
      _errorStatus = null;
    });
    try {
      await session.api.runBackupNow();
      if (!mounted) return;
      await _load();
    } on ApiException catch (e) {
      if (!mounted) return;
      // 409 is the server's way of saying the dump could not be made; it has
      // already recorded why on the run row, so the message is worth showing
      // rather than swallowing into a generic failure.
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
      });
    } on OfflineException {
      if (!mounted) return;
      setState(() => _error = 'Cannot reach the server.');
    } finally {
      if (mounted) setState(() => _running = false);
    }
  }

  /// Fetch a bundle and hand it to the platform.
  ///
  /// The bytes go to a real file first, and then to the share sheet — because a
  /// bundle left in app-private storage is not a backup the admin can do
  /// anything with. The sheet is where "put it in Drive", "mail it to myself",
  /// and "save it" all live.
  Future<void> _download(Map<String, dynamic> run) async {
    final filename = (run['filename'] as String?) ?? '';
    if (filename.isEmpty) return;
    final session = context.read<SessionState>();
    setState(() {
      _fetching = filename;
      _error = null;
      _errorStatus = null;
    });
    try {
      final bytes = await session.api.downloadBackup(filename);
      final dir = await getTemporaryDirectory();
      final file = File('${dir.path}/$filename');
      await file.writeAsBytes(bytes, flush: true);
      if (!mounted) return;
      await Share.shareXFiles([XFile(file.path)], subject: filename);
    } on ApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _errorStatus = e.statusCode;
      });
    } on OfflineException {
      if (!mounted) return;
      setState(() => _error = 'Cannot reach the server.');
    } finally {
      if (mounted) setState(() => _fetching = null);
    }
  }

  String _when(Map<String, dynamic> run) {
    final raw = (run['created_at'] as String?) ?? '';
    // The server sends RFC3339 in UTC; showing it as-is beats inventing a
    // timezone the phone may not be in.
    return raw.replaceFirst('T', ' ').split('.').first;
  }

  String _size(Map<String, dynamic> run) {
    final bytes = (run['bytes'] as num?)?.toInt();
    if (bytes == null) return '';
    if (bytes < 1024) return '$bytes B';
    if (bytes < 1024 * 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB';
    return '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MB';
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;

    return Scaffold(
      appBar: AppBar(title: const Text('Backups')),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : ListView(
              padding: const EdgeInsets.all(AppSpacing.md),
              children: [
                if (_error != null) ...[
                  _Banner(
                    scheme: scheme,
                    status: _errorStatus,
                    message: _error!,
                  ),
                  const SizedBox(height: AppSpacing.md),
                ],

                // --- the schedule -------------------------------------------
                Text('On a schedule', style: AppText.titleMedium),
                const SizedBox(height: AppSpacing.xs),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Back up automatically'),
                  subtitle: const Text(
                    'Only runs while this server is up. A host timer keeps '
                    'working when it is not.',
                  ),
                  value: _enabled,
                  onChanged: (v) => setState(() {
                    _enabled = v;
                    _cadence ??=
                        (_presets.isNotEmpty ? (_presets.first['seconds'] as num).toInt() : 86400);
                  }),
                ),
                if (_enabled) ...[
                  DropdownButtonFormField<int>(
                    initialValue: _cadence,
                    decoration: const InputDecoration(labelText: 'How often'),
                    items: _presets
                        .map((p) => DropdownMenuItem<int>(
                              value: (p['seconds'] as num).toInt(),
                              child: Text((p['label'] as String?) ?? ''),
                            ))
                        .toList(),
                    onChanged: (v) => setState(() => _cadence = v),
                  ),
                  const SizedBox(height: AppSpacing.md),
                ],
                DropdownButtonFormField<int>(
                  initialValue: _keep,
                  decoration: const InputDecoration(
                    labelText: 'Keep',
                    helperText: 'Older bundles are deleted once this many remain',
                  ),
                  items: const [7, 14, 30, 90]
                      .map((n) => DropdownMenuItem<int>(
                            value: n,
                            child: Text('$n bundles'),
                          ))
                      .toList(),
                  onChanged: (v) => setState(() => _keep = v ?? _keep),
                ),
                const SizedBox(height: AppSpacing.sm),
                Align(
                  alignment: Alignment.centerLeft,
                  child: FilledButton.icon(
                    onPressed: _saving ? null : _save,
                    icon: _saving
                        ? const SizedBox(
                            width: 16,
                            height: 16,
                            child: CircularProgressIndicator(strokeWidth: 2),
                          )
                        : const Icon(Icons.save_outlined),
                    label: const Text('Save schedule'),
                  ),
                ),

                const SizedBox(height: AppSpacing.lg),

                // --- the button ---------------------------------------------
                Text('Right now', style: AppText.titleMedium),
                const SizedBox(height: AppSpacing.xs),
                Text(
                  'A backup is a complete copy of everything this troop has. '
                  'Everyone who downloads one should be someone the troop '
                  'would hand its records to.',
                  style: AppText.bodySmall.copyWith(color: scheme.outline),
                ),
                const SizedBox(height: AppSpacing.sm),
                FilledButton.icon(
                  onPressed: _running ? null : _runNow,
                  icon: _running
                      ? const SizedBox(
                          width: 16,
                          height: 16,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : const Icon(Icons.backup_outlined),
                  label: Text(_running ? 'Backing up…' : 'Back up now'),
                ),

                const SizedBox(height: AppSpacing.lg),

                // --- the bundles --------------------------------------------
                Text('Bundles', style: AppText.titleMedium),
                if (_directory.isNotEmpty)
                  Padding(
                    padding: const EdgeInsets.only(top: AppSpacing.xs),
                    child: Text(
                      _directory,
                      style: AppText.bodySmall.copyWith(color: scheme.outline),
                    ),
                  ),
                const SizedBox(height: AppSpacing.xs),
                if (_runs.isEmpty)
                  Padding(
                    padding: const EdgeInsets.symmetric(vertical: AppSpacing.md),
                    child: Text(
                      'No bundles yet.',
                      style: AppText.bodyMedium.copyWith(color: scheme.outline),
                    ),
                  ),
                for (final run in _runs) _runTile(run, scheme),
              ],
            ),
    );
  }

  Widget _runTile(Map<String, dynamic> run, ColorScheme scheme) {
    final filename = (run['filename'] as String?) ?? '';
    final status = (run['status'] as String?) ?? 'done';
    final trigger = (run['trigger'] as String?) ?? '';
    final unrecorded = run['unrecorded'] == true;
    final failed = status == 'failed';
    final busy = _fetching == filename;

    final subtitle = StringBuffer(_when(run));
    if (trigger.isNotEmpty) subtitle.write(' · $trigger');
    final size = _size(run);
    if (size.isNotEmpty) subtitle.write(' · $size');
    if (unrecorded) {
      // Said plainly rather than folded into the list. This bundle was produced
      // by the host's own timer while the server was down, which is the state
      // that arrangement exists to produce — but nobody recorded it, and that is
      // worth knowing before restoring from it.
      subtitle.write(' · not recorded by this server');
    }

    return ListTile(
      contentPadding: EdgeInsets.zero,
      leading: Icon(
        failed
            ? Icons.error_outline
            : (unrecorded ? Icons.help_outline : Icons.inventory_2_outlined),
        color: failed ? scheme.error : null,
      ),
      title: Text(filename.isEmpty ? '(unnamed)' : filename),
      subtitle: Text(
        failed ? '${subtitle.toString()}\n${run['error'] ?? ''}'.trim() : subtitle.toString(),
      ),
      isThreeLine: failed,
      trailing: failed || filename.isEmpty
          ? null
          : IconButton(
              tooltip: 'Download',
              onPressed: busy ? null : () => _download(run),
              icon: busy
                  ? const SizedBox(
                      width: 18,
                      height: 18,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Icon(Icons.download_outlined),
            ),
    );
  }
}

/// The server's answer, shown as the server's answer.
class _Banner extends StatelessWidget {
  const _Banner({
    required this.scheme,
    required this.status,
    required this.message,
  });

  final ColorScheme scheme;
  final int? status;
  final String message;

  @override
  Widget build(BuildContext context) {
    final refused = status == 403;
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(AppSpacing.md),
      decoration: BoxDecoration(
        color: refused ? scheme.surfaceContainerHighest : scheme.errorContainer,
        borderRadius: BorderRadius.circular(AppRadius.md),
      ),
      child: Text(
        refused
            ? 'Your account does not hold `core:backup`. Ask a chief to grant '
                'it — backups are a grant of their own, deliberately not part '
                'of being an admin.'
            : message,
        style: AppText.bodySmall,
      ),
    );
  }
}
