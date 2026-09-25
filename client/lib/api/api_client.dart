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
      if (response.body.isEmpty) return null;
      // A 204 or an empty body is not JSON; guards the common case of a route
      // that succeeds without a payload.
      final text = response.body.trim();
      if (text.isEmpty) return null;
      try {
        return jsonDecode(text);
      } on FormatException {
        return text;
      }
    }

    throw ApiException(response.statusCode, _errorMessage(response));
  }

  /// The server answers errors as `{"error": "..."}`; fall back to the raw body.
  String _errorMessage(http.Response response) {
    try {
      final decoded = jsonDecode(response.body);
      if (decoded is Map && decoded['error'] is String) return decoded['error'] as String;
      if (decoded is Map && decoded['message'] is String) return decoded['message'] as String;
    } on FormatException {
      // fall through
    }
    return response.body.isEmpty ? 'Request failed' : response.body;
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

  // --- governance ---------------------------------------------------------

  Future<List<Map<String, dynamic>>> motions() async =>
      _asList(await _send('GET', '/api/governance/motions'));

  Future<Map<String, dynamic>> motion(String id) async =>
      _asMap(await _send('GET', '/api/governance/motion/$id'));

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
    for (final key in const ['items', 'data', 'members', 'missions', 'events', 'motions']) {
      final inner = value[key];
      if (inner is List) {
        return inner.whereType<Map>().map((e) => Map<String, dynamic>.from(e)).toList();
      }
    }
  }
  return const [];
}
