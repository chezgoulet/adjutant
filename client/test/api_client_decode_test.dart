import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

/// The collection routes do not all answer with a bare array, and this one's
/// wrapper was not in the list `_asList` accepts: against a real server
/// `GET /api/membership/lodges` answers `{"lodges": [...]}`, so `lodges()`
/// returned an empty list on a database that had one. The live harness
/// (`live/live_client_test.dart`) is what found it; this keeps the decode path
/// guarded without needing a server.
http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

void main() {
  test('ApiClient.lodges decodes the wrapper the server actually sends',
      () async {
    final api = ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((request) async {
        expect(request.method, 'GET');
        expect(request.url.path, '/api/membership/lodges');
        // The shape `membership` answers with: the lodge, and its patrols
        // nested inside it.
        return _json({
          'lodges': [
            {
              'id': 1,
              'name': 'Harness Lodge',
              'patrols': [
                {'id': 1, 'name': 'Harness Patrol'},
              ],
            },
          ],
        });
      }),
    );
    addTearDown(api.close);

    final lodges = await api.lodges();

    expect(lodges, hasLength(1));
    expect(lodges.first['name'], 'Harness Lodge');
  });
}
