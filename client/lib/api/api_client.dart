/// The one place the client talks to the server.
///
/// Design rule (docs/design/client-and-plugin-ui.md §4.5): the client sends the
/// same requests a human would. There is no backend-only shortcut, no privileged
/// path. Every call here is an ordinary API request subject to the route gate.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io' show SocketException;

import 'package:http/http.dart' as http;

/// Raised for any non-2xx response, carrying the server's message so the UI can
/// show a real reason instead of a generic failure.
class ApiException implements Exception {
  ApiException(this.statusCode, this.message);

  final int statusCode;
  final String message;

  bool get isUnauthorized => statusCode == 401;
  bool get isForbidden => statusCode == 403;
  bool get isNotFound => statusCode == 404;

  @override
  String toString() => 'ApiException($statusCode): $message';
}

/// Raised when the server cannot be reached at all — the offline case, which is
/// a normal state for this app, not an error to apologise for.
class OfflineException implements Exception {
  OfflineException([this.cause]);

  final Object? cause;

  @override
  String toString() => 'OfflineException($cause)';
}

/// Thin JSON client over the Adjutant HTTP API.
class ApiClient {
  ApiClient({required this.baseUrl, http.Client? httpClient})
      : _http = httpClient ?? http.Client();

  /// e.g. `https://adjutant.chezgoulet.org` — no trailing slash.
  final String baseUrl;
  final http.Client _http;

  /// Bearer token from login. Null until authenticated.
  String? _token;

  String? get token => _token;
  bool get hasToken => _token != null && _token!.isNotEmpty;

  void setToken(String? token) => _token = token;

  Uri _uri(String path, [Map<String, String>? query]) =>
      Uri.parse('$baseUrl$path').replace(queryParameters: query);

  Map<String, String> _headers({bool json = false}) => {
        if (json) 'content-type': 'application/json',
        'accept': 'application/json',
        if (hasToken) 'authorization': 'Bearer $_token',
      };

  Future<dynamic> _send(
    String method,
    String path, {
    Map<String, String>? query,
    Object? body,
  }) async {
    final uri = _uri(path, query);
    late http.Response response;
    try {
      final request = http.Request(method, uri)..headers.addAll(_headers(json: body != null));
      if (body != null) request.body = jsonEncode(body);
      final streamed = await _http.send(request).timeout(const Duration(seconds: 20));
      response = await http.Response.fromStream(streamed);
    } on SocketException catch (e) {
      throw OfflineException(e);
    } on TimeoutException catch (e) {
      throw OfflineException(e);
    } on http.ClientException catch (e) {
      throw OfflineException(e);
    }

    if (response.statusCode >= 200 && response.statusCode < 300) {
      if (response.bodyBytes.isEmpty) return null;
      // A 204 or an empty body is not JSON; guards the common case of a route
      // that succeeds without a payload.
      final text = _bodyText(response).trim();
      if (text.isEmpty) return null;
      try {
        return jsonDecode(text);
      } on FormatException {
        return text;
      }
    }

    throw ApiException(response.statusCode, _errorMessage(response));
  }

  /// The response body as text.
  ///
  /// JSON on the wire is UTF-8 by specification (RFC 8259 §8.1), whatever the
  /// content-type header happens to say — and `http`'s default, with no charset
  /// stated, is latin1. Left alone that turns “Procès-verbal” into mojibake and
  /// mangles every em dash in a notice body, so the bytes are decoded here
  /// rather than trusted to a guess.
  String _bodyText(http.Response response) {
    try {
      return utf8.decode(response.bodyBytes);
    } on FormatException {
      // Not valid UTF-8: fall back to whatever the client guessed rather than
      // losing the response entirely.
      return response.body;
    }
  }

  /// The server answers errors as `{"error": "..."}`; fall back to the raw body.
  String _errorMessage(http.Response response) {
    final text = _bodyText(response);
    try {
      final decoded = jsonDecode(text);
      if (decoded is Map && decoded['error'] is String) return decoded['error'] as String;
      if (decoded is Map && decoded['message'] is String) return decoded['message'] as String;
    } on FormatException {
      // fall through
    }
    return text.isEmpty ? 'Request failed' : text;
  }

  // --- auth ---------------------------------------------------------------

  Future<String> login({required String username, required String password}) async {
    final data = await _send('POST', '/api/auth/login',
        body: {'username': username, 'password': password});
    final token = (data is Map ? data['token'] : null) as String?;
    if (token == null || token.isEmpty) {
      throw ApiException(500, 'Login succeeded but no token was returned');
    }
    _token = token;
    return token;
  }

  Future<void> logout() async {
    try {
      await _send('POST', '/api/auth/logout');
    } on ApiException {
      // A failed logout must not trap the user in a signed-in shell; the local
      // token is cleared by the caller regardless.
    } finally {
      _token = null;
    }
  }

  Future<Map<String, dynamic>> me() async =>
      _asMap(await _send('GET', '/api/auth/me'));

  // --- membership ---------------------------------------------------------

  Future<List<Map<String, dynamic>>> members({bool includeInactive = false}) async =>
      _asList(await _send('GET', '/api/membership/members',
          query: includeInactive ? {'include_inactive': '1'} : null));

  Future<List<Map<String, dynamic>>> lodges() async =>
      _asList(await _send('GET', '/api/membership/lodges'));

  Future<List<Map<String, dynamic>>> stewards() async =>
      _asList(await _send('GET', '/api/membership/stewards'));

  // --- missions -----------------------------------------------------------

  Future<List<Map<String, dynamic>>> missions({String? stage}) async =>
      _asList(await _send('GET', '/api/missions/missions',
          query: stage == null ? null : {'stage': stage}));

  Future<Map<String, dynamic>> mission(String id) async =>
      _asMap(await _send('GET', '/api/missions/mission/$id'));

  Future<Map<String, dynamic>> impactReport() async =>
      _asMap(await _send('GET', '/api/missions/impact'));

  // --- calendar -----------------------------------------------------------

  Future<List<Map<String, dynamic>>> events({String? status}) async =>
      _asList(await _send('GET', '/api/calendar/events',
          query: status == null ? null : {'status': status}));

  Future<List<Map<String, dynamic>>> upcomingEvents() async =>
      _asList(await _send('GET', '/api/calendar/upcoming'));

  Future<Map<String, dynamic>> event(String id) async =>
      _asMap(await _send('GET', '/api/calendar/event/$id'));

  Future<Map<String, dynamic>> rsvp(String eventId, String response) async =>
      _asMap(await _send('POST', '/api/calendar/event/$eventId/rsvp',
          body: {'response': response}));

  // --- announcements ------------------------------------------------------

  /// The caller's inbox — every announcement whose scope addresses them.
  ///
  /// Visibility is the server's rule, not a filter the client can guess: an
  /// announcement is addressed to a scope, so a Lodge reader does not see a
  /// troop-wide notice unless they hold a troop-scope grant. `status` defaults
  /// to `published` server-side (`all` also returns drafts and retracted ones),
  /// expired notices are hidden unless `includeExpired`, and urgent sorts first.
  Future<List<Map<String, dynamic>>> announcements({
    String? status,
    String? category,
    bool unreadOnly = false,
    bool includeExpired = false,
  }) async {
    final query = <String, String>{
      'status': ?status,
      'category': ?category,
      if (unreadOnly) 'unread': 'true',
      if (includeExpired) 'include_expired': 'true',
    };
    return _asList(await _send('GET', '/api/announcements/announcements',
        query: query.isEmpty ? null : query));
  }

  /// The unread badge on its own: `unread`, `urgent_unread`, `has_urgent`,
  /// `unread_by_category` and the scopes the caller is addressed by.
  Future<Map<String, dynamic>> unreadAnnouncements() async =>
      _asMap(await _send('GET', '/api/announcements/unread'));

  /// One announcement, plus this caller's receipt and the read count.
  ///
  /// The server answers 403 for an announcement not sent to a scope the caller
  /// holds — the same answer as one that does not exist, which is the point.
  Future<Map<String, dynamic>> announcement(String id) async =>
      _asMap(await _send('GET', '/api/announcements/announcement/$id'));

  /// Record the caller's own receipt. Idempotent: marking read twice writes no
  /// second receipt and returns `already_read: true`.
  ///
  /// Returns the response, whose `unread` is the fresh badge.
  Future<Map<String, dynamic>> markAnnouncementRead(String id) async =>
      _asMap(await _send('POST', '/api/announcements/announcement/$id/read',
          body: {'via': 'flutter'}));

  /// Clear the caller's own receipt. Forgetting twice is not an error.
  Future<Map<String, dynamic>> markAnnouncementUnread(String id) async =>
      _asMap(await _send('POST', '/api/announcements/announcement/$id/unread'));

  // --- finance: dues ------------------------------------------------------

  /// One scout's dues: the assessment, the standing derived from the ledger
  /// (`paid_cents`, `outstanding_cents`, `settled`) and the dues payments.
  ///
  /// Your own record needs only `finance:read` at any scope — an ownership
  /// check, not a grant — which is the whole reason the client asks for itself
  /// by id rather than for "me": the route takes a member, and yours is the one
  /// you may read. Somebody else's needs `finance:read_all` covering the troop,
  /// and the server, not this client, decides that.
  Future<Map<String, dynamic>> memberDues(String member, {int? fiscalYear}) async =>
      _asMap(await _send(
          'GET', '/api/finance/dues/member/${Uri.encodeComponent(member)}',
          query: fiscalYear == null ? null : {'fiscal_year': '$fiscalYear'}));

  /// The sliding scale for the troop's configured membership cost: each tier's
  /// share, what it assesses, and the sentence a scout reads when choosing.
  ///
  /// A constant table server-side (no database call), which is what makes it
  /// the honest source for the tier chooser rather than labels hardcoded here.
  Future<Map<String, dynamic>> slidingScale() async =>
      _asMap(await _send('GET', '/api/finance/sliding-scale'));

  /// Report your own sliding-scale tier — the honor system's one write.
  ///
  /// No `member_id` is sent, so the subject is the caller and nothing else. A
  /// self-report never sets the base cost it is a fraction of: that is the
  /// treasurer's number, and the server reuses the existing assessment's base
  /// or the troop's configured membership cost, answering 409 when neither
  /// exists rather than inventing a price.
  Future<Map<String, dynamic>> selfReportDues({
    required String tier,
    int? fiscalYear,
    String? note,
  }) async {
    final body = <String, Object>{
      'tier': tier,
      'fiscal_year': ?fiscalYear,
      if (note != null && note.trim().isNotEmpty) 'note': note.trim(),
    };
    return _asMap(
        await _send('POST', '/api/finance/dues/self-report', body: body));
  }

  // --- governance ---------------------------------------------------------

  Future<List<Map<String, dynamic>>> motions() async =>
      _asList(await _send('GET', '/api/governance/motions'));

  Future<Map<String, dynamic>> motion(String id) async =>
      _asMap(await _send('GET', '/api/governance/motion/$id'));

  // --- core: plugins (admin) ----------------------------------------------

  /// Every loaded plugin with its runtime state, plus the retired-library count
  /// and the plugin ids that currently have bound event subscriptions.
  ///
  /// `core:admin`, troop scope. The core answers 401/403 to anyone else, and
  /// that refusal *is* the answer: the client does not guess which roles hold
  /// the permission, because role → permission lives in `core.role_permissions`
  /// and only the core reads it (the same reason announcements asks the
  /// permission service rather than the table).
  Future<Map<String, dynamic>> plugins() async =>
      _asMap(await _send('GET', '/api/plugins'));

  /// Enable or disable one plugin by id.
  ///
  /// Disabling aborts its routes and its event subscriptions until it is
  /// enabled again — nothing is deleted. The core refuses with 409 to disable
  /// the only enabled identity provider, because with the dev-header stub off
  /// that would make every authenticated route unreachable, including the one
  /// that would undo it.
  Future<void> setPluginEnabled(String id, bool enabled) async {
    final verb = enabled ? 'enable' : 'disable';
    await _send('POST', '/api/plugins/${Uri.encodeComponent(id)}/$verb');
  }

  void close() => _http.close();
}

// --- decoding helpers -------------------------------------------------------

Map<String, dynamic> _asMap(dynamic value) {
  if (value is Map<String, dynamic>) return value;
  if (value is Map) return Map<String, dynamic>.from(value);
  return const {};
}

/// The API returns bare arrays for collections, but has been observed to wrap
/// them (`{"items": [...]}`, `{"members": [...]}`) on some routes. Accept both
/// rather than trusting one shape — and never crash on the other.
List<Map<String, dynamic>> _asList(dynamic value) {
  if (value is List) {
    return value.whereType<Map>().map((e) => Map<String, dynamic>.from(e)).toList();
  }
  if (value is Map) {
    for (final key in const [
      'items',
      'data',
      'members',
      'missions',
      'events',
      'motions',
      'announcements',
    ]) {
      final inner = value[key];
      if (inner is List) {
        return inner.whereType<Map>().map((e) => Map<String, dynamic>.from(e)).toList();
      }
    }
  }
  return const [];
}
