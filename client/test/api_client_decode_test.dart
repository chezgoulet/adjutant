import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

/// Every collection route, decoded against the payload the server actually
/// builds — not a bare array invented for the test.
///
/// The original one-key repair for `lodges` treated the symptom: `_asList`'s
/// whitelist grew by one and the sibling routes kept the identical latent bug.
/// `GET /api/calendar/upcoming` answers `{"occurrences": …}`, which was not on
/// the list, so `upcomingEvents()` returned `[]` against a real server while
/// every test stayed green (the live harness is what found lodges originally).
///
/// Each payload below is copied from the plugin's own `json!` construction, and
/// the comment names the file and route it comes from. A route whose wrapper
/// changes will fail here rather than silently returning "none".
http.Response _json(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

/// Drive one route and assert the request it made.
Future<List<Map<String, dynamic>>> _decode(
  String path,
  Object payload,
  Future<List<Map<String, dynamic>>> Function(ApiClient) call,
) async {
  final api = ApiClient(
    baseUrl: 'http://example.test',
    httpClient: MockClient((request) async {
      expect(request.method, 'GET');
      expect(request.url.path, path);
      return _json(payload);
    }),
  );
  addTearDown(api.close);
  return call(api);
}

void main() {
  test('ApiClient.lodges decodes the wrapper the server actually sends',
      () async {
    // plugins/membership/src/lib.rs, GET /api/membership/lodges:
    //   {"lodges": rows} where each row is the lodge plus its patrols.
    final lodges = await _decode(
      '/api/membership/lodges',
      {
        'lodges': [
          {
            'id': 1,
            'name': 'Harness Lodge',
            'patrols': [
              {'id': 1, 'name': 'Harness Patrol'},
            ],
          },
        ],
      },
      (api) => api.lodges(),
    );

    expect(lodges, hasLength(1));
    expect(lodges.first['name'], 'Harness Lodge');
  });

  test('ApiClient.members decodes the roster wrapper', () async {
    // plugins/membership/src/lib.rs, GET /api/membership/members:
    //   {"members": rows} from the roster SELECT.
    final members = await _decode(
      '/api/membership/members',
      {
        'members': [
          {
            'id': 12,
            'username': 'bea',
            'email': 'bea@example.test',
            'display_name': 'Bea Otter',
            'trail_name': 'Otter',
            'osg_id': null,
            'bg_check': true,
            'is_active': true,
            'patrol': 'Harness Patrol',
            'lodge': 'Harness Lodge',
          },
        ],
      },
      (api) => api.members(),
    );

    expect(members, hasLength(1));
    expect(members.single['display_name'], 'Bea Otter');
  });

  test('ApiClient.stewards decodes {"stewards": ...} — the sibling of lodges',
      () async {
    // plugins/membership/src/lib.rs, GET /api/membership/stewards:
    //   {"stewards": rows} (the issue's PROBE 2 returned 0 of 1 here).
    final stewards = await _decode(
      '/api/membership/stewards',
      {
        'stewards': [
          {
            'id': 3,
            'position': 'Scribe',
            'appointed_on': '2026-09-01',
            'vacant': false,
            'display_name': 'Bea Otter',
            'trail_name': 'Otter',
            'lodge': 'Harness Lodge',
          },
        ],
      },
      (api) => api.stewards(),
    );

    expect(stewards, hasLength(1));
    expect(stewards.single['position'], 'Scribe');
  });

  test('ApiClient.missions decodes the missions page', () async {
    // plugins/missions/src/lib.rs, GET /api/missions/missions:
    //   {"missions": rows, "scope": "troop" | "scoped"}.
    final missions = await _decode(
      '/api/missions/missions',
      {
        'missions': [
          {
            'id': 8,
            'title': 'Ridge cleanup',
            'stage': 'planned',
            'state': 'open',
            'category': 'service',
            'lodge_id': 1,
            'lodge_name': 'Harness Lodge',
            'starts_on': '2026-10-24',
            'ends_on': '2026-10-25',
            'service_hours': 6,
            'participant_count': 4,
            'created_by': 'u1',
            'created_at': '2026-09-01 10:00:00+00',
          },
        ],
        'scope': 'troop',
      },
      (api) => api.missions(),
    );

    expect(missions, hasLength(1));
    expect(missions.single['title'], 'Ridge cleanup');
  });

  test('ApiClient.events decodes the events page', () async {
    // plugins/calendar/src/lib.rs, GET /api/calendar/events:
    //   {"events": rows, "scope": ..., "status": ...} where rows are
    //   EVENT_FIELDS.
    final events = await _decode(
      '/api/calendar/events',
      {
        'events': [
          {
            'id': 21,
            'title': 'Lodge meeting',
            'description': '',
            'category': 'meeting',
            'scope_type': 'troop',
            'scope_id': null,
            'body': 'lodge',
            'meeting_id': null,
            'location': 'The House',
            'timezone': 'America/Toronto',
            'all_day': false,
            'rrule': '',
            'exdates': '',
            'starts_at': '2026-10-22 23:00:00+00',
            'starts_local': '2026-10-22T19:00:00',
            'ends_at': '2026-10-23 01:00:00+00',
            'ends_local': '2026-10-22T21:00:00',
            'now_local': '2026-10-20T12:00:00',
            'status': 'scheduled',
            'quorum_basis': '',
            'expected_voters': 0,
            'quorum_required': 0,
            'source_mission_id': null,
            'cancelled_at': null,
            'cancelled_by': null,
            'created_by': 'u1',
            'created_at': '2026-09-01 10:00:00+00',
            'updated_at': '2026-09-01 10:00:00+00',
          },
        ],
        'scope': 'troop',
        'status': 'all',
      },
      (api) => api.events(),
    );

    expect(events, hasLength(1));
    expect(events.single['title'], 'Lodge meeting');
  });

  test('ApiClient.upcomingEvents decodes {"occurrences": ...}', () async {
    // plugins/calendar/src/lib.rs, GET /api/calendar/upcoming. This is #167:
    // the route answers occurrences + seasonal, never a bare array, and the
    // screen caches the decoded answer.
    final upcoming = await _decode(
      '/api/calendar/upcoming',
      {
        'window_days': 21,
        'occurrences': [
          {
            'event_id': 21,
            'title': 'Lodge meeting',
            'category': 'meeting',
            'scope_type': 'troop',
            'scope_id': null,
            'body': 'lodge',
            'meeting_id': null,
            'location': 'The House',
            'timezone': 'America/Toronto',
            'all_day': false,
            'recurring': false,
            'occurrence_local': '2026-10-22T19:00:00',
            'starts_local': '2026-10-22T19:00:00',
            'starts_at': '2026-10-22 23:00:00+00',
            'is_series_start': true,
          },
        ],
        'seasonal': [
          {
            'key': 'gear_check',
            'date': '2026-10-24',
            'season': 'autumn',
            'title': 'Check the gear',
            'detail': 'Before the cold.',
          },
        ],
        'truncated': false,
        'scope': 'troop',
      },
      (api) => api.upcomingEvents(),
    );

    expect(upcoming, hasLength(1));
    expect(upcoming.single['title'], 'Lodge meeting');
    // The seasonal prompts are a separate collection, not occurrences.
    expect(upcoming.single.containsKey('season'), isFalse);
  });

  test('ApiClient.announcements decodes the inbox page', () async {
    // plugins/announcements/src/lib.rs, GET /api/announcements/announcements:
    //   {"announcements": rows, "count", "limit", "offset", "status",
    //    "filters", "unread"}.
    final announcements = await _decode(
      '/api/announcements/announcements',
      {
        'announcements': [
          {
            'id': 7,
            'title': 'Meeting moved to Thursday',
            'body': 'The hall is closed Wednesday.',
            'category': 'urgent',
            'scope_type': 'troop',
            'scope_id': null,
            'status': 'published',
            'related_event_id': null,
            'published_at': '2026-10-20 12:00:00+00',
            'published_by': 'u1',
            'expires_at': null,
            'retracted_at': null,
            'retracted_by': null,
            'created_by': 'u1',
            'created_at': '2026-10-20 12:00:00+00',
            'updated_at': '2026-10-20 12:00:00+00',
          },
        ],
        'count': 1,
        'limit': 50,
        'offset': 0,
        'status': 'all',
        'filters': {
          'category': null,
          'scope_type': null,
          'scope_id': null,
          'include_expired': false,
          'unread': false,
        },
        'unread': {
          'member_id': 'u1',
          'visible': 1,
          'unread': 1,
          'read': 0,
          'urgent_unread': 1,
          'has_urgent': true,
          'unread_by_category': {
            'urgent': 1,
            'informational': 0,
            'event': 0,
          },
          'addressed': [],
        },
      },
      (api) => api.announcements(),
    );

    expect(announcements, hasLength(1));
    expect(announcements.single['title'], 'Meeting moved to Thursday');
  });

  test('ApiClient.motions decodes the motion list', () async {
    // plugins/governance/src/lib.rs, GET /api/governance/motions:
    //   {"motions": rows}.
    final motions = await _decode(
      '/api/governance/motions',
      {
        'motions': [
          {
            'id': 4,
            'meeting_id': 2,
            'title': 'Buy a new tent',
            'body': 'The old one leaks.',
            'category': 'finance',
            'stage': 'debate',
            'result': '',
            'threshold': 'simple_majority',
            'amends_accords': false,
            'proposed_by': 'u1',
            'seconded_by': 'u2',
            'votes_yes': 3,
            'votes_no': 1,
            'votes_abstain': 0,
            'decided_at': null,
            'implemented_at': null,
            'created_at': '2026-09-01 10:00:00+00',
          },
        ],
      },
      (api) => api.motions(),
    );

    expect(motions, hasLength(1));
    expect(motions.single['title'], 'Buy a new tent');
  });

  test('ApiClient.equipmentItems decodes the catalogue page', () async {
    // plugins/equipment/src/lib.rs, GET /api/equipment/items:
    //   {"items": rows, "count", "include_retired"} where rows are ITEM_FIELDS.
    final items = await _decode(
      '/api/equipment/items',
      {
        'items': [
          {
            'id': 5,
            'name': 'Four-season tent',
            'asset_tag': 'T-14',
            'category': 'shelter',
            'description': 'Sleeps four.',
            'condition': 'good',
            'location': 'Quartermaster',
            'acquired_on': '2024-05-01',
            'source': 'donated',
            'service_count': 3,
            'next_service_on': null,
            'status': 'available',
            'maintenance_since': null,
            'maintenance_until': null,
            'maintenance_note': '',
            'replacement_flagged': false,
            'replacement_note': '',
            'retired_at': null,
            'retired_reason': '',
            'created_by': 'u1',
            'created_at': '2024-05-01 10:00:00+00',
            'updated_at': '2026-09-01 10:00:00+00',
          },
        ],
        'count': 1,
        'include_retired': false,
      },
      (api) => api.equipmentItems(),
    );

    expect(items, hasLength(1));
    expect(items.single['asset_tag'], 'T-14');
  });

  test('ApiClient.storeItems decodes the catalogue page', () async {
    // plugins/store/src/lib.rs, GET /api/store/items:
    //   {"items": item_view(...) + scale, "count", "filters", "note"}.
    final items = await _decode(
      '/api/store/items',
      {
        'items': [
          {
            'id': 3,
            'kind': 'product',
            'sku': 'PATCH-161',
            'name': 'Troop crest',
            'category': 'uniform',
            'description': 'Embroidered crest.',
            'base_price_cents': 500,
            'base_price_display': r'$5.00',
            'currency': 'CAD',
            'fund_code': 'general',
            'equipment_item_id': null,
            'active': true,
            'created_by': 'u1',
            'created_at': '2026-09-01 10:00:00+00',
            'updated_at': '2026-09-01 10:00:00+00',
            'scale': [
              {
                'tier': 'standard',
                'label': 'Standard',
                'share_bps': 10000,
                'share_percent': '100.00%',
                'charged_cents': 500,
                'charged_display': r'$5.00',
                'funded_cents': 0,
                'funded_display': r'$0.00',
                'description': 'The full price.',
                'self_reportable': true,
                'capped_at_price': false,
              },
            ],
          },
        ],
        'count': 1,
        'filters': {
          'kind': null,
          'category': null,
          'include_inactive': false,
          'limit': 100,
        },
        'note': 'prices are the shop\'s',
      },
      (api) => api.storeItems(),
    );

    expect(items, hasLength(1));
    expect(items.single['name'], 'Troop crest');
    expect((items.single['scale'] as List), hasLength(1));
  });

  test('ApiClient.duesSessions decodes the sessions page', () async {
    // plugins/stripe/src/lib.rs, GET /api/stripe/sessions:
    //   {"sessions": rows, "count", "has_more", "next_before_id",
    //    "narrowed_to_caller", "note"}.
    final sessions = await _decode(
      '/api/stripe/sessions',
      {
        'sessions': [
          {
            'id': 9,
            'purpose': 'dues',
            'amount_cents': 12000,
            'currency': 'CAD',
            'member_id': 'u1',
            'fund_code': 'dues',
            'category': 'dues',
            'description': '2026 dues',
            'related_event_id': null,
            'dues_year': 2026,
            'status': 'completed',
            'stripe_session_id': 'cs_test_123',
            'checkout_url': 'https://checkout.stripe.test/cs_test_123',
            'created_by': 'u1',
            'created_at': '2026-09-01 10:00:00+00',
            'updated_at': '2026-09-01 10:05:00+00',
          },
        ],
        'count': 1,
        'has_more': false,
        'next_before_id': null,
        'narrowed_to_caller': true,
        'note': 'you are seeing the sessions you opened or that name you',
      },
      (api) => api.duesSessions(),
    );

    expect(sessions, hasLength(1));
    expect(sessions.single['stripe_session_id'], 'cs_test_123');
  });

  test('an unrecognised single-array wrapper is unwrapped structurally',
      () async {
    // The class, not the key: a new collection route with a name this function
    // has never seen is still one array-valued field.
    final items = await _decode(
      '/api/calendar/events',
      {
        'widgets': [
          {'id': 1, 'name': 'structural'},
        ],
        'scope': 'troop',
      },
      (api) => api.events(),
    );

    expect(items, hasLength(1));
    expect(items.single['name'], 'structural');
  });

  test('a payload with two unrecognised arrays is not guessed at', () async {
    // Two arrays and no named wrapper: picking one would hand the caller the
    // wrong collection. Empty is honest, and a decode test per route means this
    // cannot be the real shape of a route we ship.
    final items = await _decode(
      '/api/calendar/events',
      {
        'left': [
          {'id': 1},
        ],
        'right': [
          {'id': 2},
        ],
        'scope': 'troop',
      },
      (api) => api.events(),
    );

    expect(items, isEmpty);
  });
}
