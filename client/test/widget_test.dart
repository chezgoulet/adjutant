import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/announcement_detail_screen.dart';
import 'package:adjutant_client/screens/announcements_screen.dart';
import 'package:adjutant_client/screens/dues_screen.dart';
import 'package:adjutant_client/screens/equipment_item_screen.dart';
import 'package:adjutant_client/screens/equipment_screen.dart';
import 'package:adjutant_client/screens/governance_screen.dart';
import 'package:adjutant_client/screens/home_shell.dart';
import 'package:adjutant_client/screens/plugins_screen.dart';
import 'package:adjutant_client/screens/store_admin_screen.dart';
import 'package:adjutant_client/screens/store_item_screen.dart';
import 'package:adjutant_client/screens/store_order_screen.dart';
import 'package:adjutant_client/screens/store_orders_screen.dart';
import 'package:adjutant_client/screens/store_screen.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:adjutant_client/widgets/common.dart';
import 'package:adjutant_client/widgets/store_money.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// A response the way the server sends one: UTF-8 bytes carrying JSON.
///
/// `http.Response(String)` guesses latin1 when the content-type names no
/// charset, which cannot even hold an em dash — and the client is supposed to
/// decode JSON as the UTF-8 it is, so the fixtures are bytes like the real ones.
http.Response jsonResponse(Object body, [int status = 200]) => http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

// ---------------------------------------------------------------------------
// The shop's fixtures (SPEC §7.16)
// ---------------------------------------------------------------------------

/// One catalogue item, with a whole sliding scale — the shape
/// `GET /api/store/items` returns: the price, and for every tier both what it
/// charges **and** what it draws.
Map<String, dynamic> storeItemFixture({
  int id = 1,
  String name = 'Lodge 3 patch',
  String kind = 'product',
  int price = 2500,
  bool active = true,
  int? equipmentItemId,
}) =>
    {
      'id': id,
      'kind': kind,
      'sku': 'P-001',
      'name': name,
      'category': 'patch',
      'description': 'Embroidered lodge patch',
      'base_price_cents': price,
      'base_price_display': formatCents(price),
      'currency': 'cad',
      'fund_code': 'general',
      'equipment_item_id': equipmentItemId,
      'active': active,
      'created_by': 'u1',
      'created_at': '2026-09-01T10:00:00Z',
      'updated_at': '2026-09-01T10:00:00Z',
      'scale': [
        {
          'tier': 'patron',
          'label': 'Patron',
          'share_bps': 20000,
          'share_percent': '200%',
          'charged_cents': price,
          'charged_display': formatCents(price),
          'funded_cents': 0,
          'funded_display': r'$0.00',
          'description': 'Twice the membership cost, capped at the price',
          'self_reportable': true,
          'capped_at_price': true,
        },
        {
          'tier': 'standard',
          'label': 'Standard',
          'share_bps': 10000,
          'share_percent': '100%',
          'charged_cents': price,
          'charged_display': formatCents(price),
          'funded_cents': 0,
          'funded_display': r'$0.00',
          'description': 'The full membership cost',
          'self_reportable': true,
          'capped_at_price': false,
        },
        {
          'tier': 'supported',
          'label': 'Supported',
          'share_bps': 5000,
          'share_percent': '50%',
          'charged_cents': price ~/ 2,
          'charged_display': formatCents(price ~/ 2),
          'funded_cents': price - price ~/ 2,
          'funded_display': formatCents(price - price ~/ 2),
          'description': 'Half the membership cost',
          'self_reportable': true,
          'capped_at_price': false,
        },
        {
          'tier': 'hardship',
          'label': 'Hardship',
          'share_bps': 0,
          'share_percent': '0%',
          'charged_cents': 0,
          'charged_display': r'$0.00',
          'funded_cents': price,
          'funded_display': formatCents(price),
          'description': 'No dues this year',
          'self_reportable': true,
          'capped_at_price': false,
        },
      ],
    };

/// An order, with the three figures the money model turns on. Defaults: a
/// supported-tier purchase — charged half, funded half, draw not yet booked.
Map<String, dynamic> storeOrderFixture({
  int id = 7,
  String status = 'paid',
  int price = 2500,
  int charged = 1250,
  String drawStatus = 'unbooked',
  String ledgerStatus = 'not_attempted',
}) =>
    {
      'id': id,
      'member_id': 'u1',
      'placed_by': 'u1',
      'status': status,
      'currency': 'cad',
      'price_tier': 'supported',
      'price_cents': price,
      'charged_cents': charged,
      'funded_cents': price - charged,
      'fund_code': 'general',
      'note': '',
      'stripe_session_id': charged > 0 ? 'cs_test_123' : null,
      'checkout_url': null,
      'payment_ref': charged > 0 ? 'pi_test_123' : null,
      'ledger_status': ledgerStatus,
      'ledger_transaction_id': null,
      'ledger_error': null,
      'completed_by': status == 'paid' ? 'u2' : null,
      'completed_at': status == 'paid' ? '2026-09-25T12:00:00Z' : null,
      'comp_reason': status == 'comped' ? 'hardship, the tent was needed' : null,
      'comp_by': status == 'comped' ? 'u2' : null,
      'comp_at': null,
      'draw_status': drawStatus,
      'draw_ref': drawStatus == 'booked' ? '9c1e2f00-0001' : null,
      'draw_error': null,
      'draw_by': null,
      'draw_booked_at': drawStatus == 'booked' ? '2026-09-25T12:05:00Z' : null,
      'created_at': '2026-09-24T09:00:00Z',
      'updated_at': '2026-09-25T12:00:00Z',
    };

/// The order's `draw` block, as `draw_block()` builds it server-side.
Map<String, dynamic> drawFixture(Map<String, dynamic> order) => {
      'funded_cents': order['funded_cents'],
      'funded_display': formatCents(order['funded_cents'] as int),
      'from_fund_code': 'scholarship',
      'to_fund_code': order['fund_code'],
      'status': order['draw_status'],
      'transfer_group': order['draw_ref'],
      'error': order['draw_error'],
      'booked_at': order['draw_booked_at'],
      'mechanism': 'finance_transfer',
      'how': 'a caller holding finance:write books the draw with POST '
          '/api/store/order/{id}/draw, as themselves',
    };

/// The order's `ledger` block, as `ledger_block()` builds it server-side.
Map<String, dynamic> ledgerFixture(Map<String, dynamic> order) => {
      'status': (order['ledger_status'] as String?)?.isEmpty ?? true
          ? 'not_attempted'
          : order['ledger_status'],
      'transaction_id': order['ledger_transaction_id'],
      'error': order['ledger_error'],
      'payment_ref': order['payment_ref'],
      'mechanism': 'stripe_book',
      'note': 'finance owns every entry',
    };

/// One order line: the shop's price per unit and the charged unit kept apart,
/// exactly as the lines route returns them.
Map<String, dynamic> orderLineFixture({int quantity = 1, int listPrice = 2500}) => {
      'id': 11,
      'order_id': 7,
      'catalogue_item_id': 1,
      'item_name': 'Lodge 3 patch',
      'item_kind': 'product',
      'fund_code': 'general',
      'equipment_item_id': null,
      'list_price_cents': listPrice,
      'unit_price_cents': listPrice ~/ 2,
      'quantity': quantity,
      'line_total_cents': (listPrice ~/ 2) * quantity,
    };

// ---------------------------------------------------------------------------
// Governance's fixtures (SPEC §7.4)
// ---------------------------------------------------------------------------

/// One motion, in the shape `GET /api/governance/motions` returns it: the row
/// with its stage, its result and the recorded tally already on it.
Map<String, dynamic> motionFixture({
  int id = 7,
  int? meetingId = 3,
  String title = 'Adopt the 2027 dues schedule',
  String text = 'That the 2027 dues schedule be adopted as circulated.',
  String body = 'congress',
  String category = 'dues',
  String stage = 'voting',
  String result = '',
  String threshold = 'two_thirds',
  int yes = 3,
  int no = 1,
  int abstain = 1,
}) =>
    {
      'id': id,
      'meeting_id': meetingId,
      'title': title,
      'text': text,
      'body': body,
      'category': category,
      'stage': stage,
      'result': result,
      'threshold': threshold,
      'amends_accords': false,
      'proposed_by': 'u2',
      'seconded_by': 'u3',
      'votes_yes': yes,
      'votes_no': no,
      'votes_abstain': abstain,
      'decided_at': result.isEmpty ? null : '2026-09-26T19:00:00Z',
      'implemented_at': null,
      'created_at': '2026-09-20T09:00:00Z',
    };

/// One motion's whole page, as `GET /api/governance/motion/{id}` returns it:
/// the record, its votes, its amendments, the tally a close would produce now,
/// and the live quorum when it belongs to a meeting.
Map<String, dynamic> motionDetailFixture({
  int id = 7,
  String stage = 'voting',
  String result = '',
  String threshold = 'two_thirds',
  int yes = 3,
  int no = 1,
  int abstain = 1,
  bool wouldPass = true,
  List<Map<String, dynamic>>? votes,
  Map<String, dynamic>? quorum,
}) =>
    {
      'motion': motionFixture(
        id: id,
        stage: stage,
        result: result,
        threshold: threshold,
        yes: yes,
        no: no,
        abstain: abstain,
      ),
      'votes': votes ?? <Map<String, dynamic>>[],
      'amendments': <Map<String, dynamic>>[],
      'tally': {
        'yes': yes,
        'no': no,
        'abstain': abstain,
        'cast': yes + no,
        'would_pass': wouldPass,
        'threshold': threshold,
      },
      'quorum': quorum,
    };

/// The tests that earn their place: the pure logic the UI depends on, and the
/// two widgets whose whole job is to be legible under bad conditions.
void main() {
  group('field()', () {
    test('returns the first present, non-empty key', () {
      final map = {'title': 'Operation Slipperyskin', 'stage': 'execution'};
      expect(field(map, ['title']), 'Operation Slipperyskin');
      expect(field(map, ['missing', 'stage']), 'execution');
    });

    test('skips nulls and blank strings', () {
      final map = {'a': null, 'b': '   ', 'c': 'found'};
      expect(field(map, ['a', 'b', 'c']), 'found');
    });

    test('joins lists rather than printing them as objects', () {
      expect(field({'tags': ['hunt', 'winter']}, ['tags']), 'hunt, winter');
    });

    test('returns the fallback when nothing matches', () {
      expect(field({'a': null}, ['a', 'b'], fallback: 'Untitled'), 'Untitled');
    });
  });

  group('formatDate()', () {
    test('renders a plain date without time', () {
      expect(formatDate('2026-12-14T00:00:00Z'), matches(r'^\d{2}/\d{2}/2026$'));
    });

    test('renders 24-hour time when asked', () {
      // 18:00 UTC rendered in local time — assert the shape, not the hour, so
      // the test does not depend on the machine's timezone.
      expect(formatDate('2026-12-14T18:00:00Z', withTime: true), matches(r'^\d{2}/\d{2}/2026 \d{2}:\d{2}$'));
    });

    test('passes through unparseable input rather than crashing', () {
      expect(formatDate('not a date'), 'not a date');
      expect(formatDate(''), '—');
      expect(formatDate(null), '—');
    });
  });

  group('formatRelativeDate()', () {
    test('names today and tomorrow', () {
      final now = DateTime.now();
      expect(formatRelativeDate(now.toIso8601String()), 'Today');
      expect(formatRelativeDate(now.add(const Duration(days: 1)).toIso8601String()), 'Tomorrow');
      expect(formatRelativeDate(now.subtract(const Duration(days: 1)).toIso8601String()), 'Yesterday');
    });
  });

  group('StatusBadge', () {
    Widget wrap(Widget child, {Brightness brightness = Brightness.light}) => MaterialApp(
          theme: brightness == Brightness.dark ? AppTheme.dark() : AppTheme.light(),
          home: Scaffold(body: child),
        );

    testWidgets('states the status in words, not colour alone', (tester) async {
      await tester.pumpWidget(wrap(const StatusBadge('execution')));
      expect(find.text('In Progress'), findsOneWidget);
    });

    testWidgets('maps the lifecycle stages to human wording', (tester) async {
      for (final entry in {
        'request': 'Requested',
        'review': 'In Review',
        'approval': 'Awaiting Approval',
        'approved': 'Approved',
        'debrief': 'Debrief',
        'report': 'Report',
        'passed': 'Passed',
      }.entries) {
        await tester.pumpWidget(wrap(StatusBadge(entry.key)));
        expect(find.text(entry.value), findsOneWidget, reason: 'stage ${entry.key}');
      }
    });

    testWidgets('renders in dark mode', (tester) async {
      await tester.pumpWidget(
        wrap(const StatusBadge('approved'), brightness: Brightness.dark),
      );
      expect(find.text('Approved'), findsOneWidget);
    });
  });

  group('OfflineBanner', () {
    testWidgets('says what is happening and when the data is from', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: OfflineBanner(cachedAt: DateTime(2026, 12, 14, 18, 5)),
          ),
        ),
      );
      expect(find.textContaining('Offline'), findsOneWidget);
      expect(find.textContaining('18:05'), findsOneWidget);
    });
  });

  group('EmptyState', () {
    testWidgets('always offers a way forward, never a blank screen', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: EmptyState(
              icon: Icons.flag_outlined,
              title: 'No missions yet',
              message: 'Propose one to get started.',
              action: FilledButton(onPressed: () {}, child: const Text('Create Mission')),
            ),
          ),
        ),
      );
      expect(find.text('No missions yet'), findsOneWidget);
      expect(find.text('Create Mission'), findsOneWidget);
    });
  });

  group('ApiClient error mapping', () {
    test('carries the status code so callers can branch on it', () {
      final e = ApiException(401, 'invalid credentials');
      expect(e.isUnauthorized, isTrue);
      expect(e.isForbidden, isFalse);
      expect(e.message, 'invalid credentials');
    });

    test('exposes an offline state distinct from an error', () {
      expect(OfflineException().toString(), contains('OfflineException'));
    });
  });

  group('AppTheme', () {
    test('light and dark are both built and differ in surface', () {
      expect(AppTheme.light().brightness, Brightness.light);
      expect(AppTheme.dark().brightness, Brightness.dark);
      expect(
        AppTheme.light().colorScheme.surface,
        isNot(AppTheme.dark().colorScheme.surface),
      );
    });

    test('touch targets are never below the outdoor minimum', () {
      expect(AppSpacing.touchTargetMin, greaterThanOrEqualTo(48.0));
    });
  });

  group('Plugin administration', () {
    // The verb is the whole payload here: posting the wrong one disables a
    // plugin the operator asked to enable, which is the kind of mistake that
    // only shows up as a troop's calendar going quiet.
    test('enable and disable post the verb that was asked for', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return http.Response('{}', 200);
        }),
      );

      await client.setPluginEnabled('conflicts', true);
      await client.setPluginEnabled('conflicts', false);

      expect(calls, [
        'POST /api/plugins/conflicts/enable',
        'POST /api/plugins/conflicts/disable',
      ]);
    });

    test('a refusal arrives as a status the screen can branch on', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );

      await expectLater(
        client.plugins(),
        throwsA(
          isA<ApiException>()
              .having((e) => e.statusCode, 'statusCode', 403)
              .having((e) => e.message, 'message', 'forbidden'),
        ),
      );
    });

    test('the admin payload survives being read as a map', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"plugins":[{"id":"conflicts","name":"Conflicts","version":"0.2.0",'
            '"enabled":true,"routes":11,"kind":"native","isolated":true,'
            '"permissions":["conflicts:file"],"schedules":[],"route_list":[]}],'
            '"retired_libraries":0,"bound_subscriptions":["conflicts"]}',
            200,
          ),
        ),
      );

      final data = await client.plugins();
      expect(data['plugins'], isA<List>());
      expect((data['plugins'] as List).first['id'], 'conflicts');
      expect(data['bound_subscriptions'], ['conflicts']);
    });
  });

  group('PluginsScreen', () {
    /// Two plugins, as the core reports them.
    String payload() => jsonEncode({
          'plugins': [
            {
              'id': 'conflicts',
              'name': 'Conflicts',
              'version': '0.2.0',
              'enabled': true,
              'routes': 11,
              'kind': 'native',
              'isolated': true,
              'permissions': ['conflicts:file', 'conflicts:read_own'],
              'schedules': [],
              'route_list': [],
            },
            {
              'id': 'calendar',
              'name': 'Calendar',
              'version': '0.2.0',
              'enabled': false,
              'routes': 14,
              'kind': 'native',
              'isolated': true,
              'permissions': ['calendar:read'],
              'schedules': [],
              'route_list': [],
            },
          ],
          'retired_libraries': 0,
          'bound_subscriptions': ['conflicts'],
        });

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>.value(
          value: SessionState(client: client),
          child: const MaterialApp(home: PluginsScreen()),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('lists what is loaded, and says which are off', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((_) async => http.Response(payload(), 200)),
      );
      await pump(tester, client);

      expect(find.text('Conflicts'), findsOneWidget);
      expect(find.text('Calendar'), findsOneWidget);
      // The off one is stated, not merely greyed: an operator scans this list.
      expect(find.text('On'), findsOneWidget);
      expect(find.text('Off'), findsOneWidget);
      expect(find.textContaining('2 loaded'), findsOneWidget);
      // Facts an operator needs to make the call.
      expect(find.text('11 routes'), findsOneWidget);
      expect(find.text('isolated'), findsNWidgets(2));
      expect(find.text('subscribed'), findsOneWidget);
    });

    testWidgets('disabling asks first, then posts the disable verb', (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'GET') return http.Response(payload(), 200);
          return http.Response('{}', 200);
        }),
      );
      await pump(tester, client);

      // The first switch belongs to the first plugin — Conflicts.
      await tester.tap(find.byType(Switch).first);
      await tester.pumpAndSettle();

      // Nothing has been sent yet: taking a plugin offline is not a slip of the
      // thumb, and the dialog says what it costs.
      expect(find.text('Disable Conflicts?'), findsOneWidget);
      expect(calls.where((c) => c.startsWith('POST')), isEmpty);

      await tester.tap(find.text('Disable'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/plugins/conflicts/disable'));
    });

    testWidgets('keeping it on sends nothing at all', (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'GET') return http.Response(payload(), 200);
          return http.Response('{}', 200);
        }),
      );
      await pump(tester, client);

      await tester.tap(find.byType(Switch).first);
      await tester.pumpAndSettle();
      await tester.tap(find.text('Keep it on'));
      await tester.pumpAndSettle();

      expect(calls.where((c) => c.startsWith('POST')), isEmpty);
    });

    testWidgets('a refusal reads as admins-only, not as an error', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );
      await pump(tester, client);

      expect(find.text('Admins only'), findsOneWidget);
      expect(find.textContaining('core:admin'), findsOneWidget);
      // Not an error state: there is nothing to retry, and nothing is wrong.
      expect(find.text('Cannot reach the server'), findsNothing);
    });
  });

  group('formatCents()', () {
    test('renders cents the way the server does', () {
      expect(formatCents(0), r'$0.00');
      expect(formatCents(1250), r'$12.50');
      expect(formatCents(1234567), r'$12,345.67');
      expect(formatCents(-1250), r'-$12.50');
      expect(formatCents(null), '—');
    });

    test('groups thousands the way the finance plugin groups them', () {
      // 100_000_00 cents is $100,000.00 — grouping is a legibility rule here,
      // not decoration: a scout checking a dues figure reads it once.
      expect(formatCents(10000000), r'$100,000.00');
      expect(formatCents(99999), r'$999.99');
      expect(formatCents(100000), r'$1,000.00');
    });
  });

  group('Announcements API', () {
    test('the inbox read sends only the filters it was given', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add(request.url.toString());
          return http.Response('{"announcements":[],"count":0}', 200);
        }),
      );

      expect(await client.announcements(), isEmpty);
      await client.announcements(unreadOnly: true, category: 'urgent');

      expect(calls.first, 'http://example.test/api/announcements/announcements');
      expect(
        calls.last,
        'http://example.test/api/announcements/announcements?category=urgent&unread=true',
      );
    });

    test('a wrapped list is read as announcements, a bare one as items', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"announcements":[{"id":7,"title":"Meeting moved"}],"count":1}',
            200,
          ),
        ),
      );
      final list = await client.announcements();
      expect(list, hasLength(1));
      expect(list.first['title'], 'Meeting moved');
    });

    test('read and unread are two different receipts, and read says via', () async {
      final calls = <String>[];
      String? readBody;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'POST' && request.url.path.endsWith('/read')) {
            readBody = request.body;
          }
          return http.Response(
            '{"is_read":true,"already_read":false,"unread":{"unread":0}}',
            200,
          );
        }),
      );

      await client.markAnnouncementRead('7');
      await client.markAnnouncementUnread('7');

      expect(calls, [
        'POST /api/announcements/announcement/7/read',
        'POST /api/announcements/announcement/7/unread',
      ]);
      expect(jsonDecode(readBody!), {'via': 'flutter'});
    });

    test('the badge is read from the server, never computed here', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"member_id":"u1","visible":4,"unread":2,"read":2,"urgent_unread":1,'
            '"has_urgent":true,"unread_by_category":{"urgent":1},'
            '"addressed":{"troop":true,"lodges":[]}}',
            200,
          ),
        ),
      );

      final badge = await client.unreadAnnouncements();
      expect(badge['unread'], 2);
      expect(badge['has_urgent'], true);
    });
  });

  group('Dues API', () {
    test('the standing is read for one member, by id', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add(request.url.toString());
          return http.Response('{"member_id":"u1","dues":null,"payments":[]}', 200);
        }),
      );

      await client.memberDues('8f0a-1b2c');
      await client.memberDues('u1', fiscalYear: 2026);

      expect(
        calls.first,
        'http://example.test/api/finance/dues/member/8f0a-1b2c',
      );
      expect(
        calls.last,
        'http://example.test/api/finance/dues/member/u1?fiscal_year=2026',
      );
    });

    test('a self-report names the tier and never a member', () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return http.Response(
            '{"dues":{"tier":"supported"},"assessed_display":"\$75.00"}',
            200,
          );
        }),
      );

      await client.selfReportDues(tier: 'supported');

      // The subject is the caller: sending a member_id would be claiming the
      // right to report for somebody else, which needs finance:manage_dues.
      expect(body, {'tier': 'supported'});
    });

    test('paying opens a Stripe session for what is owed, as the member', () async {
      String? path;
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          path = request.url.path;
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return http.Response(
            '{"checkout_url":"https://checkout.stripe.test/cs_1",'
            '"session":{"id":7},"checkout":{"amount_cents":10000}}',
            201,
          );
        }),
      );

      final response = await client.payDues(
        amountCents: 10000,
        duesYear: 2026,
        memberId: 'u1',
      );

      expect(path, '/api/stripe/checkout');
      // The member names only themselves; finance:write is never touched — dues
      // move by stripe's payment being booked, not by this call.
      expect(body, {
        'purpose': 'dues',
        'amount_cents': 10000,
        'dues_year': 2026,
        'member_id': 'u1',
      });
      expect(response['checkout_url'], 'https://checkout.stripe.test/cs_1');
    });

    test('the payment sessions are read narrowed to the caller', () async {
      String? url;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          url = request.url.toString();
          return http.Response(
            '{"sessions":[{"id":7,"status":"completed","amount_cents":10000}],'
            '"count":1,"narrowed_to_caller":true}',
            200,
          );
        }),
      );

      final sessions = await client.duesSessions(memberId: 'u1');

      expect(
        url,
        'http://example.test/api/stripe/sessions?purpose=dues&member_id=u1',
      );
      expect(sessions.single['status'], 'completed');
    });

    test('the scale is read whole, with the amounts the server assessed', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"base_cents":15000,"base_display":"\$150.00","base_configured":true,'
            '"minimum_cents":0,"tiers":[{"tier":"hardship","label":"Hardship",'
            '"assessed_cents":0,"assessed_display":"\$0.00",'
            '"description":"No dues this year","self_reportable":true}]}',
            200,
          ),
        ),
      );

      final scale = await client.slidingScale();
      final tiers = (scale['tiers'] as List).cast<Map>();
      expect(tiers.single['tier'], 'hardship');
      expect(tiers.single['assessed_display'], r'$0.00');
    });

    test('a refusal arrives as a status the screen can branch on', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );

      await expectLater(
        client.memberDues('someone-else'),
        throwsA(isA<ApiException>()
            .having((e) => e.statusCode, 'statusCode', 403)
            .having((e) => e.message, 'message', 'forbidden')),
      );
    });
  });

  group('AnnouncementsScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Map<String, dynamic> badge() => {
          'member_id': 'u1',
          'visible': 2,
          'unread': 1,
          'read': 1,
          'urgent_unread': 1,
          'has_urgent': true,
          'unread_by_category': {'urgent': 1, 'informational': 0, 'event': 0},
          'addressed': {
            'troop': true,
            'lodges': <String>[],
            'oversees': false,
            'manage_lodges': <String>[],
          },
        };

    /// Two notices: one urgent and unread, one informational and read. This is
    /// the ordinary shape of an inbox on the morning something has moved.
    List<Map<String, dynamic>> notices() => [
          {
            'id': 7,
            'title': 'Meeting moved to Thursday',
            'body': 'Lodge 3 meets Thursday instead.',
            'category': 'urgent',
            'scope_type': 'troop',
            'scope_id': null,
            'status': 'published',
            'published_at': '2026-09-24T18:00:00Z',
            'is_read': false,
          },
          {
            'id': 8,
            'title': 'Gear check',
            'body': '',
            'category': 'informational',
            'scope_type': 'lodge',
            'scope_id': '3',
            'status': 'published',
            'published_at': '2026-09-23T10:00:00Z',
            'is_read': true,
          },
        ];

    ApiClient clientWith({
      required List<String> calls,
      bool refused = false,
      int unread = 1,
      bool hasUrgent = true,
    }) =>
        ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            final path = request.url.path;
            calls.add('${request.method} $path');
            if (refused) return http.Response('{"error":"forbidden"}', 403);
            if (path == '/api/announcements/announcements') {
              return jsonResponse({
                'announcements': notices(),
                'count': 2,
                'unread': badge(),
              });
            }
            if (path == '/api/announcements/unread') {
              return jsonResponse({...badge(), 'unread': unread, 'has_urgent': hasUrgent});
            }
            if (path == '/api/announcements/announcement/7/read') {
              return http.Response(
                '{"receipt":{"id":1},"is_read":true,"already_read":false,'
                '"unread":{"unread":0,"has_urgent":false}}',
                200,
              );
            }
            return http.Response('{}', 200);
          }),
        );

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const Scaffold(body: AnnouncementsScreen()),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('lists the inbox and makes urgent say so in words', (tester) async {
      final calls = <String>[];
      await pump(tester, clientWith(calls: calls));

      expect(find.text('Meeting moved to Thursday'), findsOneWidget);
      expect(find.text('Gear check'), findsOneWidget);
      // Urgent is not rendered like the rest: the card names itself, so the
      // category survives a greyscale screen and a colour-blind reader.
      expect(find.text('Urgent'), findsOneWidget);
      expect(find.text('Urgent unread'), findsOneWidget);
      // And the count is the server's whole count, not this page's.
      expect(find.textContaining('1 unread of 2'), findsOneWidget);
      // Each row offers the receipt it is missing.
      expect(find.text('Mark read'), findsOneWidget);
      expect(find.text('Mark unread'), findsOneWidget);
    });

    testWidgets('marking read posts the receipt, then re-reads the server',
        (tester) async {
      final calls = <String>[];
      await pump(tester, clientWith(calls: calls));

      await tester.tap(find.text('Mark read'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/announcements/announcement/7/read'));
      // The list is re-read rather than patched locally: the badge and the
      // receipt count are the server's to state.
      expect(
        calls.where((c) => c == 'GET /api/announcements/announcements').length,
        greaterThan(1),
      );
    });

    testWidgets('a refusal reads as not-addressed, naming the permission',
        (tester) async {
      await pump(tester, clientWith(calls: <String>[], refused: true));

      expect(find.text('Announcements are not addressed to you'), findsOneWidget);
      expect(find.textContaining('announcements:read'), findsOneWidget);
      // Nothing to retry: the server answered, and being refused is the answer.
      expect(find.text('Cannot reach the server'), findsNothing);
    });

    testWidgets('an empty inbox is never a blank screen', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/announcements/announcements') {
            return http.Response('{"announcements":[],"count":0}', 200);
          }
          return jsonResponse(badge());
        }),
      );
      await pump(tester, client);

      expect(find.text('No announcements'), findsOneWidget);
      // A way forward, and an explanation of where notices come from.
      expect(find.textContaining('Notices sent to the scopes you hold'),
          findsOneWidget);
    });

    testWidgets('every row is at or above the outdoor touch-target floor',
        (tester) async {
      await pump(tester, clientWith(calls: <String>[]));

      for (final label in ['Mark read', 'Mark unread']) {
        final size = tester.getSize(
          find.ancestor(
            of: find.text(label),
            matching: find.byType(InkWell),
          ).first,
        );
        expect(size.height, greaterThanOrEqualTo(AppSpacing.touchTargetMin),
            reason: '$label is a control a scout taps outdoors');
      }
    });
  });

  group('AnnouncementDetailScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    testWidgets('opening an unread notice writes the receipt once', (tester) async {
      final calls = <String>[];
      var marked = false;

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'POST') {
            marked = true;
            return http.Response(
              '{"receipt":{"id":1},"is_read":true,"already_read":false,'
              '"unread":{"unread":0,"has_urgent":false}}',
              200,
            );
          }
          return jsonResponse({
            'announcement': {
              'id': 7,
              'title': 'Meeting moved to Thursday',
              'body': 'Lodge 3 meets Thursday instead.',
              'category': 'urgent',
              'scope_type': 'troop',
              'status': 'published',
              'published_at': '2026-09-24T18:00:00Z',
            },
            'is_read': marked,
            'my_receipt': marked ? {'id': 1, 'read_at': '2026-09-25T07:00:00Z'} : null,
            'read_count': marked ? 4 : 3,
            'delivery': 'deferred: no push provider is wired',
          });
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const AnnouncementDetailScreen(id: '7'),
          ),
        ),
      );
      await tester.pumpAndSettle();

      expect(find.text('Meeting moved to Thursday'), findsOneWidget);
      expect(find.textContaining('Lodge 3 meets Thursday'), findsOneWidget);
      // The receipt is written once, on open — not once per rebuild.
      expect(
        calls.where((c) => c == 'POST /api/announcements/announcement/7/read').length,
        1,
      );
      // The read state and the count are the server's, restated after the write.
      expect(find.textContaining('You have read this'), findsOneWidget);
      expect(find.text('Read by 4'), findsOneWidget);
      // Who read, never who has not: the roster is not this plugin's to read.
      expect(find.textContaining('Delivered'), findsNothing);
    });

    testWidgets('a notice addressed elsewhere repeats the server refusal',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"error":"no announcement 9, or it was not sent to a scope you hold"}',
            403,
          ),
        ),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const AnnouncementDetailScreen(id: '9'),
          ),
        ),
      );
      await tester.pumpAndSettle();

      expect(find.text('Not sent to a scope you hold'), findsOneWidget);
      expect(find.textContaining('not sent to a scope you hold'), findsWidgets);
    });
  });

  group('DuesScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Map<String, dynamic> standing() => {
          'member_id': 'u1',
          'fiscal_year': 2026,
          'dues': {
            'id': 11,
            'fiscal_year': 2026,
            'dues_kind': 'member',
            'member_id': 'u1',
            'lodge_id': '3',
            'tier': 'standard',
            'share_bps': 10000,
            'base_cents': 15000,
            'assessed_cents': 15000,
            'self_reported': true,
            'status': 'self_reported',
            'note': '',
            'paid_cents': 5000,
            'outstanding_cents': 10000,
            'settled': false,
          },
          'payments': [
            {
              'id': 4,
              'amount_cents': 5000,
              'kind': 'income',
              'category': 'dues',
              'description': 'Dues 2026 — u1',
              'occurred_on': '2026-03-01',
            },
          ],
          'honor_system': true,
          'next': 'self-report a different tier',
        };

    Map<String, dynamic> scale() => {
          'base_cents': 15000,
          'base_display': r'$150.00',
          'base_configured': true,
          'minimum_cents': 0,
          'minimum_display': r'$0.00',
          'tiers': [
            {
              'tier': 'patron',
              'label': 'Patron',
              'share_bps': 20000,
              'assessed_cents': 30000,
              'assessed_display': r'$300.00',
              'description': 'Twice the membership cost',
              'self_reportable': true,
            },
            {
              'tier': 'standard',
              'label': 'Standard',
              'share_bps': 10000,
              'assessed_cents': 15000,
              'assessed_display': r'$150.00',
              'description': 'The full membership cost',
              'self_reportable': true,
            },
            {
              'tier': 'supported',
              'label': 'Supported',
              'share_bps': 5000,
              'assessed_cents': 7500,
              'assessed_display': r'$75.00',
              'description': 'Half the membership cost',
              'self_reportable': true,
            },
            {
              'tier': 'hardship',
              'label': 'Hardship',
              'share_bps': 0,
              'assessed_cents': 0,
              'assessed_display': r'$0.00',
              'description': 'No dues this year — nobody is turned away for hardship',
              'self_reportable': true,
            },
          ],
          'honor_system': true,
          'note': 'A scout reports their own tier — this software has no income '
              'verification and no field for one.',
        };

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const DuesScreen(),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('shows what I owe, as the ledger derived it', (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          return jsonResponse(standing());
        }),
      );
      await pump(tester, client);

      expect(find.text('Dues 2026'), findsOneWidget);
      // The headline is what is still owed, and the rows that explain it.
      expect(find.text('You owe'), findsOneWidget);
      expect(find.text('Assessed'), findsOneWidget);
      expect(find.text('Paid'), findsOneWidget);
      // Nothing is funded, so no "Covered" row is invented.
      expect(find.text('Covered'), findsNothing);
      expect(find.text(r'$150.00'), findsWidgets);
      // The paid total and the payment that made it are the same figure, by
      // construction: a balance is derived from the ledger, never stored twice.
      expect(find.text(r'$50.00'), findsNWidgets(2));
      expect(find.text(r'$100.00'), findsOneWidget);
      expect(find.textContaining('Not settled'), findsOneWidget);
      // The tier, and the fact that the scout said it themselves.
      expect(find.text('Standard — self-reported'), findsOneWidget);
      expect(find.text('Self-reported'), findsOneWidget);
      // A payment the treasurer booked, and the standing it moved. A list is
      // built lazily, so the page is scrolled to it rather than assumed.
      await tester.scrollUntilVisible(
        find.text('Dues 2026 — u1'),
        300,
        scrollable: find.byType(Scrollable).first,
      );
      expect(find.text('Dues 2026 — u1'), findsOneWidget);
    });

    testWidgets('a waived, funded year reads as covered — not as a price of zero',
        (tester) async {
      // What the server actually returns after a waiver: the assessment stands
      // (it is the tier's), the whole of it is funded, and `outstanding_cents`
      // and `settled` are derived from that and the ledger. The scout must read
      // "covered", never a bare "Waived" that hides who paid — and never the
      // draw's mechanics, which are the troop's business.
      Map<String, dynamic> waived() => {
            'member_id': 'u1',
            'fiscal_year': 2026,
            'dues': {
              'id': 12,
              'fiscal_year': 2026,
              'dues_kind': 'member',
              'member_id': 'u1',
              'lodge_id': '3',
              'tier': 'supported',
              'share_bps': 5000,
              'base_cents': 15000,
              'assessed_cents': 7500,
              'funded_cents': 7500,
              'draw_status': 'unbooked',
              'draw_ref': null,
              'self_reported': false,
              'status': 'waived',
              'note': '',
              'paid_cents': 0,
              'outstanding_cents': 0,
              'settled': true,
            },
            'payments': <Map<String, dynamic>>[],
            'honor_system': true,
            'next': 'no assessment to change',
          };

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          return jsonResponse(waived());
        }),
      );
      await pump(tester, client);

      // The badge and the money row both read "Covered"; "Waived" is gone.
      expect(find.text('Covered'), findsWidgets);
      expect(find.text('Waived'), findsNothing);
      // Nothing is owed, and the scout is told so, in words. The two zeroes are
      // the "you owe" headline and the paid row.
      expect(find.text('You owe'), findsOneWidget);
      expect(find.text(r'$0.00'), findsNWidgets(2));
      expect(find.textContaining('Nothing is owed'), findsOneWidget);
      expect(find.text('Settled — nothing outstanding for the year'), findsOneWidget);
      // What the troop's books say about the subsidy never reaches this screen.
      expect(find.textContaining('scholarship'), findsNothing);
      expect(find.textContaining('unbooked'), findsNothing);
      expect(find.textContaining('draw'), findsNothing);
      // And a covered member is not offered a payment: Stripe cannot take zero.
      await tester.scrollUntilVisible(
        find.text('Paying dues'),
        300,
        scrollable: find.byType(Scrollable).first,
      );
      expect(find.textContaining('nothing left to pay'), findsOneWidget);
      expect(find.textContaining('with Stripe'), findsNothing);
    });

    testWidgets('a partly-covered year states the covered part and what is left',
        (tester) async {
      // A self-reported reduction funds the discount, not the whole assessment:
      // funded $75 of a $75 assessment, nothing paid yet — so the scout owes the
      // assessment and is told which part is covered.
      Map<String, dynamic> partly() => {
            'member_id': 'u1',
            'fiscal_year': 2026,
            'dues': {
              'id': 13,
              'fiscal_year': 2026,
              'dues_kind': 'member',
              'member_id': 'u1',
              'lodge_id': '3',
              'tier': 'supported',
              'share_bps': 5000,
              'base_cents': 15000,
              'assessed_cents': 7500,
              'funded_cents': 7500,
              'draw_status': 'unbooked',
              'draw_ref': null,
              'self_reported': true,
              'status': 'self_reported',
              'note': '',
              'paid_cents': 0,
              'outstanding_cents': 7500,
              'settled': false,
            },
            'payments': <Map<String, dynamic>>[],
            'honor_system': true,
            'next': 'self-report a different tier',
          };

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          return jsonResponse(partly());
        }),
      );
      await pump(tester, client);

      expect(find.text('You owe'), findsOneWidget);
      expect(find.text(r'$75.00'), findsWidgets);
      expect(find.text('Covered'), findsWidgets);
      expect(find.textContaining('the covered part of your dues is paid for you'),
          findsOneWidget);
      expect(find.textContaining('Not settled'), findsOneWidget);
      expect(find.text('Waived'), findsNothing);
    });

    testWidgets('a member pays what they owe through a Stripe session', (tester) async {
      final calls = <String>[];
      Map<String, dynamic>? checkoutBody;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          if (request.url.path == '/api/stripe/checkout') {
            checkoutBody = jsonDecode(request.body) as Map<String, dynamic>;
            return jsonResponse({
              'checkout_url': 'https://checkout.stripe.test/cs_test_123',
              'session': {'id': 7, 'status': 'created'},
              'checkout': {'provider': 'stripe', 'mode': 'payment', 'amount_cents': 10000},
              'ledger': {'category': 'dues', 'not_yet': 'nothing is booked yet'},
            }, 201);
          }
          if (request.url.path == '/api/stripe/sessions') {
            return jsonResponse({
              'sessions': [
                {
                  'id': 7,
                  'status': 'created',
                  'amount_cents': 10000,
                  'created_at': '2026-09-26T10:00:00Z',
                }
              ],
              'count': 1,
              'narrowed_to_caller': true,
            });
          }
          return jsonResponse(standing());
        }),
      );
      await pump(tester, client);

      await tester.scrollUntilVisible(
        find.text('Paying dues'),
        300,
        scrollable: find.byType(Scrollable).first,
      );
      // The button offers exactly what is owed — the assessment less what is paid.
      expect(
        find.widgetWithText(FilledButton, r'Pay $100.00 with Stripe'),
        findsOneWidget,
      );

      await tester.tap(find.text(r'Pay $100.00 with Stripe'));
      await tester.pumpAndSettle();

      // The session is opened for what is owed, by the member for themselves —
      // no member but the caller, and nothing charged by opening it.
      expect(calls, contains('POST /api/stripe/checkout'));
      expect(checkoutBody, {
        'purpose': 'dues',
        'amount_cents': 10000,
        'dues_year': 2026,
        'member_id': 'u1',
      });
      // The link is shown to be copied, exactly as the store screen shows one —
      // this client launches no browser.
      expect(find.text('Checkout session'), findsOneWidget);
      expect(find.text('https://checkout.stripe.test/cs_test_123'), findsOneWidget);
      expect(find.text('Copy the link'), findsOneWidget);
      // The payment's own state is read back, not assumed: the standing was
      // re-read and the session says where it is.
      expect(calls, contains('GET /api/finance/dues/member/u1'));
      expect(find.text('Awaiting payment'), findsOneWidget);
    });

    testWidgets('an unconfigured Stripe is said as itself, not a broken button',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          if (request.url.path == '/api/stripe/checkout') {
            return http.Response(
              '{"error":"no Stripe secret_key is configured for this plugin"}',
              503,
              headers: {'content-type': 'application/json; charset=utf-8'},
            );
          }
          return jsonResponse(standing());
        }),
      );
      await pump(tester, client);

      await tester.scrollUntilVisible(
        find.text('Paying dues'),
        300,
        scrollable: find.byType(Scrollable).first,
      );
      await tester.tap(find.text(r'Pay $100.00 with Stripe'));
      await tester.pumpAndSettle();

      // The 503 is stated as an unconfigured troop, in the server's words. The
      // server's message appears both inline and in the snackbar.
      expect(find.textContaining('Stripe is not configured'), findsOneWidget);
      expect(find.textContaining('no Stripe secret_key is configured'), findsWidgets);
      // No checkout link is invented when no session was opened.
      expect(find.text('Checkout session'), findsNothing);
    });

    testWidgets('reporting a tier posts the tier alone, and the scale is the source',
        (tester) async {
      final calls = <String>[];
      Map<String, dynamic>? reportBody;
      var tier = 'standard';

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.url.path == '/api/finance/sliding-scale') {
            return jsonResponse(scale());
          }
          if (request.method == 'POST') {
            reportBody = jsonDecode(request.body) as Map<String, dynamic>;
            tier = reportBody!['tier'] as String;
            return http.Response(
              '{"dues":{"tier":"$tier","status":"self_reported"},'
              '"assessed_display":"\$75.00","honor_system":true,'
              '"note":"Self-reported: no income verification."}',
              200,
            );
          }
          final data = standing();
          (data['dues'] as Map<String, dynamic>)['tier'] = tier;
          return jsonResponse(data);
        }),
      );
      await pump(tester, client);

      await tester.tap(find.text('Change my tier'));
      await tester.pumpAndSettle();

      // The sheet offers the tiers the server stated, with the server's amounts.
      expect(find.text('The sliding scale'), findsOneWidget);
      expect(find.text('Patron'), findsOneWidget);
      expect(find.text(r'$75.00'), findsOneWidget);
      expect(find.text('Hardship'), findsOneWidget);

      await tester.tap(find.text('Supported'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/finance/dues/self-report'));
      expect(reportBody, {'tier': 'supported'});
      // The contract is own-record-only: no `member_id` is sent, so the subject
      // is the caller and nothing else (naming another member needs
      // finance:manage_dues, which a scout does not hold).
      expect(reportBody!.containsKey('member_id'), isFalse);
      // The server's answer is what the screen reports back.
      expect(find.textContaining('assessed \$75.00'), findsOneWidget);
      expect(find.text('Supported — self-reported'), findsOneWidget);
    });

    testWidgets('the standing is served from cache, and says who told it',
        (tester) async {
      // A scout in the woods with no signal: the last-known answer, and the
      // hour it is from, and no empty ledger implying nothing is owed.
      SharedPreferences.setMockInitialValues({
        'cache.dues.u1': jsonEncode(standing()),
        'cache.dues.u1.at': DateTime(2026, 9, 25, 6, 30).toIso8601String(),
        'cache.dues.scale': jsonEncode(scale()),
        'cache.dues.scale.at': DateTime(2026, 9, 25, 6, 30).toIso8601String(),
      });

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => throw http.ClientException('no route to host'),
        ),
      );
      await pump(tester, client);

      expect(find.textContaining('Offline'), findsOneWidget);
      expect(find.textContaining('06:30'), findsOneWidget);
      expect(find.text(r'$100.00'), findsOneWidget);
      expect(find.text('Standard — self-reported'), findsOneWidget);
    });

    testWidgets('a refusal is stated as a refusal, not as an empty record',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"forbidden"}', 403),
        ),
      );
      await pump(tester, client);

      expect(find.text('Not yours to read'), findsOneWidget);
      expect(find.textContaining('finance:read_all'), findsOneWidget);
      expect(find.byType(EmptyState), findsOneWidget);
    });
  });

  group('HomeShell', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    testWidgets('the inbox is a destination, badged with the server count',
        (tester) async {
      // A phone, because that is where the shell has to hold six destinations
      // above the 48dp touch target floor.
      tester.view.physicalSize = const Size(1170, 2532);
      tester.view.devicePixelRatio = 3.0;
      addTearDown(tester.view.reset);

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          if (path == '/api/announcements/unread') {
            return http.Response(
              '{"member_id":"u1","visible":2,"unread":3,"read":0,'
              '"urgent_unread":1,"has_urgent":true,'
              '"unread_by_category":{"urgent":1},'
              '"addressed":{"troop":true,"lodges":[]}}',
              200,
            );
          }
          if (path == '/api/announcements/announcements') {
            return http.Response(
              '{"announcements":[{"id":7,"title":"Meeting moved to Thursday",'
              '"category":"urgent","scope_type":"troop","scope_id":null,'
              '"status":"published","published_at":"2026-09-24T18:00:00Z",'
              '"is_read":false}],"count":1}',
              200,
            );
          }
          // Everything the dashboard asks for, empty.
          return http.Response('[]', 200);
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: const MaterialApp(home: HomeShell()),
        ),
      );
      await tester.pumpAndSettle();

      // Daily work sits in the navigation, not behind Settings.
      expect(find.text('Inbox'), findsOneWidget);
      expect(find.text('Shop'), findsOneWidget);
      expect(find.text('Equipment'), findsOneWidget);
      expect(find.text('Settings'), findsOneWidget);
      // Eight destinations rather than the five the design language prefers, so
      // each one is still a 48dp-wide target on a 390dp phone: that floor is
      // the one that matters outdoors, and it is what makes the extra items
      // affordable rather than cramped. The shop and equipment joined it for
      // the same reason the inbox did — they are ordinary troop work, not
      // settings.
      final destinations =
          tester.widgetList(find.byType(NavigationDestination)).length;
      expect(destinations, 8);
      final barWidth = tester.getSize(find.byType(NavigationBar)).width;
      expect(
        barWidth / destinations,
        greaterThanOrEqualTo(AppSpacing.touchTargetMin),
        reason: 'each destination must stay a 48dp target on a 390dp phone',
      );
      // The badge is the server's count (3 unread), and it is the urgent one.
      expect(find.byType(Badge), findsOneWidget);
      expect(find.text('3'), findsOneWidget);

      await tester.tap(find.text('Inbox'));
      await tester.pumpAndSettle();

      // Tapping it opens the inbox itself, with no second tap needed.
      expect(find.text('Meeting moved to Thursday'), findsOneWidget);
    });

    testWidgets('the shop is a destination, and opens the catalogue',
        (tester) async {
      tester.view.physicalSize = const Size(1170, 2532);
      tester.view.devicePixelRatio = 3.0;
      addTearDown(tester.view.reset);

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          if (path == '/api/announcements/unread') {
            return jsonResponse({
              'member_id': 'u1',
              'unread': 0,
              'has_urgent': false,
              'addressed': {'troop': true, 'lodges': <String>[]},
            });
          }
          if (path == '/api/store/items') {
            return jsonResponse({
              'items': [storeItemFixture()],
              'count': 1,
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: const MaterialApp(home: HomeShell()),
        ),
      );
      await tester.pumpAndSettle();

      await tester.tap(find.text('Shop'));
      await tester.pumpAndSettle();

      expect(find.text('Lodge 3 patch'), findsOneWidget);
      // The shop's operator surfaces are not in the scout's navigation: they
      // are behind Settings, where the drawer for things that need changing is.
      expect(find.text('Unsettled'), findsNothing);
    });

    testWidgets('equipment is a destination, and opens the gear pool',
        (tester) async {
      tester.view.physicalSize = const Size(1170, 2532);
      tester.view.devicePixelRatio = 3.0;
      addTearDown(tester.view.reset);

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          if (path == '/api/announcements/unread') {
            return jsonResponse({
              'member_id': 'u1',
              'unread': 0,
              'has_urgent': false,
              'addressed': {'troop': true, 'lodges': <String>[]},
            });
          }
          if (path == '/api/equipment/items') {
            return jsonResponse({
              'items': [
                {
                  'id': 1,
                  'name': 'Camp tent',
                  'asset_tag': 'AT-001',
                  'category': 'tent',
                  'condition': 'good',
                  'status': 'available',
                },
              ],
              'count': 1,
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: const MaterialApp(home: HomeShell()),
        ),
      );
      await tester.pumpAndSettle();

      await tester.tap(find.text('Equipment'));
      await tester.pumpAndSettle();

      // A scout reaches the gear pool without a curl: the catalogue, and from
      // it the availability and the checkout log, are one tap from the shell.
      expect(find.text('Camp tent'), findsOneWidget);
      expect(find.text('Catalogue'), findsOneWidget);
      expect(find.text('Available'), findsOneWidget);
      expect(find.text('I have out'), findsOneWidget);
    });
  });

  // -------------------------------------------------------------------------
  // The shop (SPEC §7.16)
  // -------------------------------------------------------------------------

  group('Store API', () {
    test('the catalogue read sends only the filters it was given', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add(request.url.toString());
          return jsonResponse({'items': [], 'count': 0});
        }),
      );

      expect(await client.storeItems(), isEmpty);
      await client.storeItems(kind: 'rental', category: 'gear');

      expect(calls.first, 'http://example.test/api/store/items');
      expect(calls.last,
          'http://example.test/api/store/items?kind=rental&category=gear');
    });

    test('an item is read out of its wrapper', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({'item': storeItemFixture()}),
        ),
      );

      final item = await client.storeItem('1');
      expect(item['name'], 'Lodge 3 patch');
      // The whole scale arrives with it: each tier's charge and its draw.
      final scale = (item['scale'] as List).cast<Map>();
      expect(scale, hasLength(4));
      expect(
        scale.firstWhere((row) => row['tier'] == 'hardship')['funded_cents'],
        2500,
      );
    });

    test('placing an order sends lines and a tier, and never a price',
        () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({'order': storeOrderFixture(status: 'open')});
        }),
      );

      await client.placeStoreOrder(
        lines: [
          {'item_id': 1, 'quantity': 2},
        ],
        tier: 'supported',
      );

      // The shop prices from its catalogue: no amount the client could send
      // would be read, and none is sent.
      expect(body, {
        'lines': [
          {'item_id': 1, 'quantity': 2},
        ],
        'tier': 'supported',
      });
      expect(body!['price_cents'], isNull);
      expect(body!['charged_cents'], isNull);
    });

    test('checkout posts to the order, with no body of its own', () async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          return jsonResponse({
            'order': storeOrderFixture(status: 'awaiting_payment'),
            'checkout_url': 'https://checkout.stripe.test/cs_test_123',
          });
        }),
      );

      final response = await client.checkoutStoreOrder('7');
      expect(calls, ['POST /api/store/order/7/checkout']);
      expect(response['checkout_url'], 'https://checkout.stripe.test/cs_test_123');
    });

    test('a comp carries its mandatory reason, and a draw its own verb',
        () async {
      final calls = <String>[];
      final bodies = <String, Map<String, dynamic>>{};
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          bodies[request.url.path] =
              jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({'order': storeOrderFixture(status: 'comped')});
        }),
      );

      await client.compStoreOrder('7', reason: 'hardship');
      await client.bookStoreDraw('7');

      expect(calls, [
        'POST /api/store/order/7/comp',
        'POST /api/store/order/7/draw',
      ]);
      // A comp is an authority, not a price: the reason is not optional.
      expect(bodies['/api/store/order/7/comp'], {'reason': 'hardship'});
      expect(bodies['/api/store/order/7/draw'], isEmpty);
    });

    test('completing names the stripe payment it is completed against',
        () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({'order': storeOrderFixture()});
        }),
      );

      await client.completeStoreOrder('7', stripePaymentId: 5);
      expect(body, {'stripe_payment_id': 5});
    });

    test('the worklist is read whole: its orders, its reasons, its note',
        () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({
            'orders': [
              {...storeOrderFixture(status: 'awaiting_payment'),
                'unsettled_reason': 'awaiting_payment',
                'total_unsettled': 1},
            ],
            'count': 1,
            'total_unsettled': 1,
            'older_than_minutes': 30,
            'by_reason': {'awaiting_payment': 1},
            'note': 'an order awaiting payment cannot be told from one this '
                'plugin cannot see paid',
          }),
        ),
      );

      final page = await client.unsettledStoreOrders();
      expect(page['total_unsettled'], 1);
      expect(page['older_than_minutes'], 30);
      expect((page['by_reason'] as Map)['awaiting_payment'], 1);
      expect((page['orders'] as List).single['unsettled_reason'],
          'awaiting_payment');
    });

    test('the orders page says whether it was narrowed to the caller',
        () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({
            'orders': [storeOrderFixture()],
            'count': 1,
            'has_more': false,
            'narrowed_to_caller': true,
            'note': 'your own orders; anybody else\'s needs store:read_all',
          }),
        ),
      );

      final page = await client.storeOrders();
      expect(page['narrowed_to_caller'], true);
      expect((page['orders'] as List).single['price_cents'], 2500);
    });

    test('a refusal arrives as a status the screen can branch on', () async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response(
            '{"error":"requires store:read_all at scope troop"}',
            403,
          ),
        ),
      );

      await expectLater(
        client.unsettledStoreOrders(),
        throwsA(isA<ApiException>()
            .having((e) => e.statusCode, 'statusCode', 403)
            .having((e) => e.message, 'message',
                'requires store:read_all at scope troop')),
      );
    });

    test('accented text and em dashes survive the wire', () async {
      // The UTF-8 decode is what stops "Procès-verbal" becoming mojibake and
      // every em dash in a description being mangled.
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({
            'item': {
              ...storeItemFixture(name: 'Fanion du Procès-verbal'),
              'description': 'Écusson brodé — Lodge 3',
            },
          }),
        ),
      );

      final item = await client.storeItem('1');
      expect(item['name'], 'Fanion du Procès-verbal');
      expect(item['description'], 'Écusson brodé — Lodge 3');
    });
  });

  group('StoreOrderMoney', () {
    Widget wrap(Widget child) => MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(body: SingleChildScrollView(child: child)),
        );

    testWidgets('the three figures are named, and never collapse into one',
        (tester) async {
      final order = storeOrderFixture(); // price 2500, charged 1250, funded 1250
      await tester.pumpWidget(wrap(StoreOrderMoney(order: order)));

      expect(find.text('Price'), findsOneWidget);
      expect(find.text('Charged'), findsOneWidget);
      expect(find.text('Funded'), findsOneWidget);

      final price = tester.widget<StoreMoneyRow>(
        find.ancestor(
          of: find.text('Price'),
          matching: find.byType(StoreMoneyRow),
        ),
      );
      final charged = tester.widget<StoreMoneyRow>(
        find.ancestor(
          of: find.text('Charged'),
          matching: find.byType(StoreMoneyRow),
        ),
      );
      final funded = tester.widget<StoreMoneyRow>(
        find.ancestor(
          of: find.text('Funded'),
          matching: find.byType(StoreMoneyRow),
        ),
      );

      // The charged amount is not the price, and the funded amount is a draw:
      // each figure is its own number, read from the server's own field.
      expect(price.cents, 2500);
      expect(charged.cents, 1250);
      expect(funded.cents, 1250);
      expect(charged.cents, isNot(price.cents));
      expect(
        find.textContaining('not a discount off the price'),
        findsOneWidget,
      );
    });

    testWidgets('a comp is stated as a subsidy, never as free money',
        (tester) async {
      final order = storeOrderFixture(
        status: 'comped',
        charged: 0,
        drawStatus: 'booked',
      );
      await tester.pumpWidget(wrap(StoreOrderMoney(order: order)));

      final charged = tester.widget<StoreMoneyRow>(
        find.ancestor(
          of: find.text('Charged'),
          matching: find.byType(StoreMoneyRow),
        ),
      );
      expect(charged.cents, 0);
      // The whole price is drawn from a real fund, and the sentence says so.
      expect(
        find.textContaining('the whole price is a draw on the scholarship fund'),
        findsOneWidget,
      );
      expect(find.textContaining('free order'), findsOneWidget);
    });

    testWidgets('a fully paid order funds nothing, and says so', (tester) async {
      await tester.pumpWidget(
        wrap(StoreOrderMoney(
          order: storeOrderFixture(charged: 2500, drawStatus: 'none'),
        )),
      );

      expect(find.textContaining('Nothing was funded'), findsOneWidget);
      expect(find.text(r'$25.00'), findsNWidgets(2)); // price and charged
      expect(find.text(r'$0.00'), findsOneWidget); // funded
    });
  });

  group('StoreDrawSection', () {
    testWidgets('an unbooked draw is shown as money that has not landed',
        (tester) async {
      final order = storeOrderFixture(drawStatus: 'unbooked');
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: SingleChildScrollView(
              child: StoreDrawSection(draw: drawFixture(order)),
            ),
          ),
        ),
      );

      expect(find.text('Scholarship draw'), findsOneWidget);
      expect(find.text('Not booked yet'), findsOneWidget);
      expect(
        find.textContaining('out of the scholarship fund into the general fund'),
        findsOneWidget,
      );
      // It is on the worklist until it lands, and the screen says so rather
      // than rendering a funded amount as settled.
      expect(
        find.textContaining('This draw has not landed'),
        findsOneWidget,
      );
    });

    testWidgets('a booked draw says the transfer was balanced',
        (tester) async {
      final order = storeOrderFixture(drawStatus: 'booked');
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(
            body: SingleChildScrollView(
              child: StoreDrawSection(draw: drawFixture(order)),
            ),
          ),
        ),
      );

      expect(find.text('Booked'), findsOneWidget);
      expect(
        find.textContaining('the sum of every fund is unchanged'),
        findsOneWidget,
      );
      expect(find.textContaining('This draw has not landed'), findsNothing);
    });
  });

  group('StoreScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    ApiClient clientWith({required List<String> calls, bool refused = false}) =>
        ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            calls.add('${request.method} ${request.url.path}');
            if (refused) {
              return http.Response(
                '{"error":"requires store:read at scope troop"}',
                403,
              );
            }
            return jsonResponse({
              'items': [
                storeItemFixture(),
                storeItemFixture(
                  id: 2,
                  name: 'Camp tent',
                  kind: 'rental',
                  price: 4000,
                  equipmentItemId: 9,
                ),
              ],
              'count': 2,
            });
          }),
        );

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const Scaffold(body: StoreScreen()),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('lists the catalogue with the shop\'s prices', (tester) async {
      await pump(tester, clientWith(calls: <String>[]));

      expect(find.text('Lodge 3 patch'), findsOneWidget);
      expect(find.text('Camp tent'), findsOneWidget);
      expect(find.text(r'$25.00'), findsOneWidget);
      expect(find.text(r'$40.00'), findsOneWidget);
      expect(find.text('2 items for sale'), findsOneWidget);
      expect(find.text('Rental'), findsOneWidget);
      // Everyday work: the orders list is one tap away here.
      expect(find.text('My orders'), findsOneWidget);
      // And the money model is stated where the prices are.
      expect(find.textContaining('a subsidy is not free'), findsOneWidget);
      // No row says "free", because nothing in this shop is priced at nothing.
      expect(find.text('Free'), findsNothing);
    });

    testWidgets('a refusal names the permission and keeps the server words',
        (tester) async {
      await pump(tester, clientWith(calls: <String>[], refused: true));

      expect(find.text('The catalogue is not yours to read'), findsOneWidget);
      expect(find.textContaining('store:read'), findsOneWidget);
      // The server's own message is kept beside the requirement — a bare
      // "forbidden" was a defect in an earlier screen.
      expect(find.textContaining('requires store:read at scope troop'),
          findsOneWidget);
      expect(find.text('Cannot reach the server'), findsNothing);
    });
  });

  group('StoreItemScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    testWidgets('buying places an order priced by the server', (tester) async {
      // A tall viewport, so the whole item — its scale, its custody and its buy
      // action — is built without a scroll.
      tester.view.physicalSize = const Size(1000, 4200);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);

      final calls = <String>[];
      Map<String, dynamic>? orderBody;

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          if (request.method == 'POST' && request.url.path == '/api/store/order') {
            orderBody = jsonDecode(request.body) as Map<String, dynamic>;
            return jsonResponse({
              'order': storeOrderFixture(id: 7, status: 'open'),
              'lines': [orderLineFixture()],
              'draw': drawFixture(storeOrderFixture(id: 7, status: 'open')),
              'next': 'open a Checkout session with POST /api/store/order/7/checkout',
            });
          }
          if (request.url.path == '/api/store/order/7') {
            final order = storeOrderFixture(status: 'open');
            return jsonResponse({
              'order': order,
              'lines': [orderLineFixture()],
              'draw': drawFixture(order),
              'ledger': ledgerFixture(order),
            });
          }
          return jsonResponse({'item': storeItemFixture()});
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const StoreItemScreen(id: '1'),
          ),
        ),
      );
      await tester.pumpAndSettle();

      // The whole scale is visible: every tier's charge and its draw.
      expect(find.text('The sliding scale'), findsOneWidget);
      expect(find.text('Patron'), findsOneWidget);
      expect(find.text('pays \$25.00'), findsNWidgets(2)); // patron, standard
      expect(find.text('pays \$12.50'), findsOneWidget);
      expect(find.textContaining('drawn from the scholarship fund'),
          findsWidgets);

      await tester.tap(find.text('Buy'));
      await tester.pumpAndSettle();

      expect(find.text('Buy Lodge 3 patch'), findsOneWidget);
      // Nothing in the sheet asks for money: the shop prices from its
      // catalogue, and the tier only says whose share is charged.
      expect(find.text('Charged'), findsOneWidget);
      expect(find.text('Funded'), findsOneWidget);

      await tester.tap(find.text('Place the order'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/store/order'));
      expect(orderBody!['lines'], [
        {'item_id': 1, 'quantity': 1},
      ]);
      // The default is the tier that pays the whole price: a reduction is the
      // buyer's to claim, not this screen's to assume.
      expect(orderBody!['tier'], 'standard');
      expect(orderBody!.containsKey('price_cents'), isFalse);
      // The server's own `next` sentence is what the buyer is told.
      expect(
        find.textContaining('open a Checkout session'),
        findsWidgets,
      );
    });
  });

  group('StoreOrderScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    ApiClient clientFor(Map<String, dynamic> order) => ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            if (request.url.path == '/api/store/order/7') {
              return jsonResponse({
                'order': order,
                'lines': [orderLineFixture()],
                'draw': drawFixture(order),
                'ledger': ledgerFixture(order),
              });
            }
            return http.Response('{}', 200);
          }),
        );

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      // A tall viewport: this screen is a long one, and a lazy ListView does not
      // build what is below the fold. The assertions are about what the screen
      // says, not about how far a thumb has scrolled.
      tester.view.physicalSize = const Size(1000, 4200);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const StoreOrderScreen(id: '7'),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('shows the price, the charge and the draw, each named',
        (tester) async {
      final order = storeOrderFixture(); // price 2500, charged 1250, funded 1250
      await pump(tester, clientFor(order));

      expect(find.text('Order #7'), findsWidgets); // the app bar and the heading
      expect(find.text('What this order costs'), findsOneWidget);
      expect(find.text('Price'), findsOneWidget);
      expect(find.text('Charged'), findsOneWidget);
      expect(find.text('Funded'), findsOneWidget);
      expect(find.textContaining('Paid'), findsWidgets);

      // The line keeps the shop's price and the charged unit apart too.
      expect(
        find.textContaining("the shop's price was \$25.00 each"),
        findsOneWidget,
      );
      // The draw is not presented as settled: it is `unbooked`.
      expect(find.text('Scholarship draw'), findsOneWidget);
      expect(find.text('Not booked yet'), findsOneWidget);
      // And the ledger's silence is stated rather than glossed.
      expect(find.text('Not attempted'), findsOneWidget);
      expect(
        find.textContaining('the order stays on the unsettled worklist'),
        findsOneWidget,
      );
    });

    testWidgets('a comped order offers no payment, only the draw',
        (tester) async {
      final order = storeOrderFixture(
        status: 'comped',
        charged: 0,
        drawStatus: 'booked',
      );
      await pump(tester, clientFor(order));

      expect(
        find.textContaining('This order charges nothing'),
        findsOneWidget,
      );
      expect(find.textContaining('Stripe cannot take zero'), findsOneWidget);
      // The draw is settled, so there is nothing left to book.
      expect(find.textContaining('nothing left to settle for it'),
          findsOneWidget);
      expect(find.text('Booked'), findsWidgets);
    });

    testWidgets('the operator acts name their permissions in words',
        (tester) async {
      final order = storeOrderFixture(status: 'open', ledgerStatus: 'unbooked');
      await pump(tester, clientFor(order));

      // Completing and comping an open order are both offered; neither is
      // hidden behind a client-side guess at the reader's roles.
      expect(find.text('Complete against a stripe payment'), findsOneWidget);
      expect(find.text('Comp this order (no charge)'), findsOneWidget);
      expect(find.textContaining('store:manage'), findsOneWidget);
      expect(find.textContaining('store:comp'), findsOneWidget);
      // The draw section's own sentence names it too — the authority a draw
      // needs is finance's, not the shop's.
      expect(find.textContaining('finance:write'), findsWidgets);
    });

    testWidgets('a refusal repeats the server, and names the permission',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => http.Response('{"error":"no such order"}', 403),
        ),
      );
      await pump(tester, client);

      expect(find.text('Not yours to read'), findsOneWidget);
      expect(find.textContaining('store:read_all'), findsOneWidget);
      // The server answers the same way for an order that is not yours and one
      // that does not exist, so its words are repeated rather than replaced.
      expect(find.textContaining('no such order'), findsOneWidget);
    });
  });

  group('StoreOrdersScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Future<void> pump(
      WidgetTester tester, {
      required bool narrowed,
      List<Map<String, dynamic>>? orders,
    }) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient(
          (_) async => jsonResponse({
            'orders': orders ?? [storeOrderFixture()],
            'count': (orders ?? [storeOrderFixture()]).length,
            'narrowed_to_caller': narrowed,
            'note': narrowed
                ? 'your own orders; anybody else\'s needs store:read_all'
                : 'the troop\'s orders: what was sold, to whom, and what it was charged',
          }),
        ),
      );
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const StoreOrdersScreen(),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('renders the three figures on every row', (tester) async {
      await pump(tester, narrowed: true);

      expect(find.text('Order #7'), findsOneWidget);
      expect(find.text('Price'), findsOneWidget);
      expect(find.text('Charged'), findsOneWidget);
      expect(find.text('Funded'), findsOneWidget);
      expect(find.text(r'$25.00'), findsOneWidget);
      expect(find.text(r'$12.50'), findsNWidgets(2)); // charged and funded
      expect(
        find.textContaining('a draw, not a price of zero'),
        findsOneWidget,
      );
    });

    testWidgets('the scope of the list is the server\'s answer', (tester) async {
      await pump(tester, narrowed: true);
      // Narrowed: the screen says whose orders these are, in the server's terms.
      expect(find.textContaining('These are your own orders'), findsOneWidget);
      expect(find.textContaining('store:read_all'), findsWidgets);
    });

    testWidgets('a widened list says it was widened, not narrowed',
        (tester) async {
      await pump(tester, narrowed: false, orders: [storeOrderFixture()]);
      expect(find.textContaining('This is the troop'), findsOneWidget);
      expect(find.textContaining('These are your own orders'), findsNothing);
    });
  });

  group('StoreAdminScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    ApiClient clientWith({
      required List<String> calls,
      bool refused = false,
      List<Map<String, dynamic>>? unsettled,
      List<Map<String, dynamic>>? comps,
    }) =>
        ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            final path = request.url.path;
            calls.add('${request.method} $path');
            if (refused) {
              return http.Response(
                '{"error":"requires store:read_all at scope troop"}',
                403,
              );
            }
            if (path == '/api/store/orders/unsettled') {
              return jsonResponse({
                'orders': unsettled ??
                    [
                      {
                        ...storeOrderFixture(status: 'awaiting_payment'),
                        'unsettled_reason': 'awaiting_payment',
                        'total_unsettled': 1,
                      },
                    ],
                'count': 1,
                'total_unsettled': 1,
                'older_than_minutes': 30,
                'by_reason': {'awaiting_payment': 1},
                'note': 'an order awaiting payment cannot be told from one this '
                    'plugin cannot see paid, because a Stripe webhook confirms '
                    'the payment to stripe and carries no Adjutant caller',
              });
            }
            if (path == '/api/store/comps') {
              return jsonResponse({
                'comps': comps ??
                    [
                      {
                        ...storeOrderFixture(status: 'comped', charged: 0),
                        'comp_reason': 'hardship — the tent was needed',
                      },
                    ],
                'count': 1,
                'funded_total_cents': 2500,
                'funded_total_display': r'$25.00',
                'note': 'each comp charged nothing and drew its whole price '
                    'from the scholarship fund',
              });
            }
            return http.Response('{}', 200);
          }),
        );

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client),
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const StoreAdminScreen(),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('the worklist shows what is unsettled, by reason',
        (tester) async {
      final calls = <String>[];
      await pump(tester, clientWith(calls: calls));

      expect(find.text('1 unsettled'), findsOneWidget);
      expect(find.text('older than 30m'), findsOneWidget);
      expect(find.text('Awaiting payment'), findsWidgets);
      expect(find.text('Order #7'), findsOneWidget);
      // The three figures, and the reason it is on the list.
      expect(
        find.textContaining('Price \$25.00 · charged \$12.50 · funded \$12.50'),
        findsOneWidget,
      );
      // The server's own note says why the two shapes are counted together.
      expect(
        find.textContaining('cannot be told from one this plugin cannot see paid'),
        findsOneWidget,
      );
      expect(calls, contains('GET /api/store/orders/unsettled'));
    });

    testWidgets('a refusal states the permission and the server message',
        (tester) async {
      await pump(tester, clientWith(calls: <String>[], refused: true));

      expect(find.text('The worklist is for operators'), findsOneWidget);
      expect(find.textContaining('store:read_all'), findsOneWidget);
      expect(find.textContaining('requires store:read_all at scope troop'),
          findsOneWidget);
      expect(find.text('Cannot reach the server'), findsNothing);
    });

    testWidgets('the comps tab shows the reason, the authority and the total',
        (tester) async {
      final calls = <String>[];
      await pump(tester, clientWith(calls: calls));

      await tester.tap(find.text('Comps'));
      await tester.pumpAndSettle();

      expect(calls, contains('GET /api/store/comps'));
      expect(find.text('Funded \$25.00'), findsOneWidget);
      expect(
        find.textContaining('hardship — the tent was needed'),
        findsOneWidget,
      );
      expect(
        find.textContaining('By u2'),
        findsOneWidget,
      );
      // The ledger shows the draw; this shows the comp, and it says so.
      expect(
        find.textContaining('The ledger shows the draw; this shows the comp'),
        findsOneWidget,
      );
    });

    testWidgets('adding an item names the permission it needs',
        (tester) async {
      await pump(tester, clientWith(calls: <String>[]));

      await tester.tap(find.text('New item'));
      await tester.pumpAndSettle();

      expect(find.textContaining('Adding an item needs store:manage'),
          findsOneWidget);
      expect(find.text('Add the item'), findsOneWidget);
      // No amount field invites a "free" item: a zero price and a comp are
      // different things, and the helper text says which is which.
      expect(
        find.textContaining("never a member's charge"),
        findsOneWidget,
      );
    });
  });

  // -------------------------------------------------------------------------
  // Equipment (SPEC §7.6) — the gear pool, from a scout's side
  // -------------------------------------------------------------------------

  Map<String, dynamic> equipmentItemFixture({
    int id = 1,
    String name = 'Camp tent',
    String status = 'available',
    String condition = 'good',
    String category = 'tent',
  }) =>
      {
        'id': id,
        'name': name,
        'asset_tag': 'AT-00$id',
        'category': category,
        'description': 'Four-season, two-person',
        'condition': condition,
        'location': 'Q-store',
        'acquired_on': '2024-05-01',
        'source': null,
        'service_count': 3,
        'next_service_on': null,
        'status': status,
        'maintenance_since': null,
        'maintenance_until': null,
        'maintenance_note': null,
        'replacement_flagged': false,
        'replacement_note': null,
        'retired_at': null,
        'retired_reason': null,
        'created_by': 'u1',
        'created_at': '2026-09-01T10:00:00Z',
        'updated_at': '2026-09-01T10:00:00Z',
      };

  /// One open checkout row, as `GET /api/equipment/checkouts` returns it.
  Map<String, dynamic> openCheckoutFixture({
    int id = 5,
    int itemId = 3,
    String itemName = 'Rope 50m',
    String dueOn = '2026-09-28',
    bool open = true,
  }) =>
      {
        'id': id,
        'item_id': itemId,
        'item_name': itemName,
        'asset_tag': 'AT-00$itemId',
        'category': 'rope',
        'checked_out_by': 'u1',
        'checked_out_at': '2026-09-20T09:00:00Z',
        'checked_out_on': '2026-09-20',
        'due_on': dueOn,
        'purpose': 'Weekend camp',
        'mission_id': null,
        'destination': 'Mount Mansfield',
        'condition_out': 'good',
        'note_out': '',
        'checked_in_at': open ? null : '2026-09-25T09:00:00Z',
        'checked_in_on': open ? null : '2026-09-25',
        'checked_in_by': open ? null : 'u1',
        'condition_in': open ? null : 'good',
        'note_in': '',
        'damaged': false,
        'open': open,
      };

  /// A full availability page: one item free, one in maintenance, one already
  /// out to somebody else across the window.
  Map<String, dynamic> availabilityFixture() => {
        'from': '2026-09-26',
        'to': '2026-10-03',
        'days': 7,
        'today': '2026-09-26',
        'available': [
          equipmentItemFixture(id: 1, name: 'Camp tent'),
        ],
        'unavailable': [
          {
            'item': equipmentItemFixture(
              id: 2,
              name: 'Bear canister',
              status: 'maintenance',
            ),
            'reasons': ['in_maintenance'],
            'blocking': <Map<String, dynamic>>[],
          },
          {
            'item': equipmentItemFixture(id: 3, name: 'Rope 50m'),
            'reasons': ['checked_out'],
            'blocking': [
              {
                'checkout_id': 5,
                'checked_out_on': '2026-09-20',
                'due_on': '2026-09-28',
                'held_by': 'u2',
                'purpose': 'Weekend camp',
                'mission_id': null,
                'condition_out': 'good',
                'open': true,
                'overdue': false,
              },
            ],
          },
        ],
        'out': <Map<String, dynamic>>[],
        'counts': {
          'available': 1,
          'unavailable': 2,
          'in_pool': 3,
          'by_reason': {'in_maintenance': 1, 'checked_out': 1},
        },
        'thresholds': {'maintenance_lead_days': 14, 'overdue_grace_days': 0},
        'notice': 'An item is available when it is in the pool, is not '
            'unserviceable, and no checkout touches the window.',
      };

  group('Equipment API', () {
    test('the availability read carries the window it was given', () async {
      final calls = <String>[];
      Map<String, String>? query;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add('${request.method} ${request.url.path}');
          query = request.url.queryParameters;
          return jsonResponse(availabilityFixture());
        }),
      );

      await client.equipmentAvailability(from: '2026-09-26', to: '2026-10-03');

      expect(calls, ['GET /api/equipment/availability']);
      expect(query?['from'], '2026-09-26');
      expect(query?['to'], '2026-10-03');
      // No filter the caller did not ask for is invented on the way out.
      expect(query?['category'], isNull);
    });

    test('the checkout body keeps only what the caller supplied', () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({'checkout': {}, 'item': {}}, 201);
        }),
      );

      await client.checkoutEquipmentItem(
        '7',
        dueOn: '2026-10-03',
        purpose: 'Fall camp',
        missionId: 9,
        condition: 'good',
      );

      expect(body?['due_on'], '2026-10-03');
      expect(body?['mission_id'], 9);
      expect(body?['condition'], 'good');
      // The holder defaults to the caller; the client never names one.
      expect(body?.containsKey('checked_out_by'), isFalse);
      expect(body?.containsKey('destination'), isFalse);
    });

    test('a checkin always states its condition', () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({'checkout': {}, 'item': {}});
        }),
      );

      await client.checkinEquipmentItem('7', condition: 'fair', damaged: true);

      expect(body?['condition'], 'fair');
      expect(body?['damaged'], true);
    });
  });

  group('EquipmentScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Future<void> pump(
      WidgetTester tester,
      ApiClient client, {
      Size size = const Size(1000, 2400),
    }) async {
      tester.view.physicalSize = size;
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const Scaffold(body: EquipmentScreen()),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    ApiClient clientFor({
      required List<String> calls,
      bool refused = false,
      List<Map<String, dynamic>>? checkouts,
      String today = '2026-09-26',
    }) =>
        ApiClient(
          baseUrl: 'http://example.test',
          httpClient: MockClient((request) async {
            final path = request.url.path;
            calls.add('${request.method} $path');
            if (refused) {
              return http.Response(
                '{"error":"requires equipment:read at scope troop"}',
                403,
              );
            }
            if (path == '/api/equipment/items') {
              return jsonResponse({
                'items': [
                  equipmentItemFixture(),
                  equipmentItemFixture(
                    id: 2,
                    name: 'Bear canister',
                    status: 'maintenance',
                  ),
                ],
                'count': 2,
              });
            }
            if (path == '/api/equipment/availability') {
              return jsonResponse(availabilityFixture());
            }
            if (path == '/api/equipment/checkouts') {
              return jsonResponse({
                'checkouts': checkouts ?? [openCheckoutFixture()],
                'count': (checkouts ?? [openCheckoutFixture()]).length,
                'open': (checkouts ?? [openCheckoutFixture()]).length,
                'state': 'open',
                'overdue_only': false,
                'today': today,
              });
            }
            return http.Response('[]', 200);
          }),
        );

    testWidgets('the catalogue lists what the troop owns', (tester) async {
      final calls = <String>[];
      await pump(tester, clientFor(calls: calls));

      expect(calls, contains('GET /api/equipment/items'));
      expect(find.text('Camp tent'), findsOneWidget);
      expect(find.text('Bear canister'), findsOneWidget);
      expect(find.text('AT-001'), findsOneWidget);
      expect(find.text('Condition: Good'), findsWidgets);
      // A retired item is not offered unless asked for; the server owns that,
      // and the control states what it does.
      expect(find.text('Include retired'), findsOneWidget);
    });

    testWidgets('availability shows both sides, with the refusal in words',
        (tester) async {
      await pump(tester, clientFor(calls: <String>[]));

      await tester.tap(find.text('Available'));
      await tester.pumpAndSettle();

      expect(find.text('Available (1)'), findsOneWidget);
      expect(find.text('Not available (2)'), findsOneWidget);
      expect(find.text('Camp tent'), findsOneWidget);
      // The reason is the useful part — in words, with the server's code kept
      // beside it so the two cannot drift.
      expect(
        find.text('In maintenance — out of the pool for service'),
        findsOneWidget,
      );
      expect(find.text('in_maintenance'), findsOneWidget);
      // And when a checkout blocks it, who holds it and the promise.
      expect(
        find.textContaining('Checked out — somebody already has it'),
        findsOneWidget,
      );
      expect(find.textContaining('Held by u2 — due back 28/09/2026'),
          findsOneWidget);
    });

    testWidgets('a refusal on availability names the permission and the words',
        (tester) async {
      await pump(tester, clientFor(calls: <String>[], refused: true));

      await tester.tap(find.text('Available'));
      await tester.pumpAndSettle();

      expect(find.text('Availability is not yours to read'), findsOneWidget);
      expect(find.textContaining('equipment:read'), findsOneWidget);
      expect(find.textContaining('requires equipment:read at scope troop'),
          findsOneWidget);
      expect(find.text('Cannot reach the server'), findsNothing);
    });

    testWidgets('what I am holding shows an open checkout and its due date',
        (tester) async {
      final calls = <String>[];
      await pump(tester, clientFor(calls: calls));

      await tester.tap(find.text('I have out'));
      await tester.pumpAndSettle();

      // Narrowed to the caller with the server's own vocabulary.
      expect(calls, contains('GET /api/equipment/checkouts'));
      expect(find.text('Rope 50m'), findsOneWidget);
      expect(find.text('Due 28/09/2026'), findsOneWidget);
      expect(find.text('Out since 20/09/2026'), findsOneWidget);
      // On time: no overdue badge.
      expect(find.text('Overdue'), findsNothing);
    });

    testWidgets('an overdue open checkout is flagged as overdue', (tester) async {
      await pump(
        tester,
        clientFor(
          calls: <String>[],
          checkouts: [openCheckoutFixture(dueOn: '2026-09-20')],
          today: '2026-09-26',
        ),
      );

      await tester.tap(find.text('I have out'));
      await tester.pumpAndSettle();

      expect(find.text('Overdue'), findsOneWidget);
      expect(find.text('Was due 20/09/2026'), findsOneWidget);
    });
  });

  group('EquipmentItemScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      tester.view.physicalSize = const Size(1000, 2600);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const EquipmentItemScreen(id: '1'),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('a checkout posts the mission the app knows and the grade',
        (tester) async {
      final calls = <String>[];
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          calls.add('${request.method} $path');
          if (path == '/api/missions/missions') {
            return jsonResponse({
              'missions': [
                {'id': 9, 'title': 'Operation Slipperyskin', 'stage': 'execution'},
              ],
            });
          }
          if (path == '/api/equipment/item/1/checkout') {
            body = jsonDecode(request.body) as Map<String, dynamic>;
            return jsonResponse({'checkout': {}, 'item': {}, 'notes': 'ok'}, 201);
          }
          if (path == '/api/equipment/item/1') {
            return jsonResponse({
              'item': equipmentItemFixture(),
              'open_checkout': null,
              'checkouts': <Map<String, dynamic>>[],
              'flags': {'replacement': {'candidate': false, 'reasons': []}},
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      await tester.tap(find.text('Check out'));
      await tester.pumpAndSettle();

      // The mission is a choice from the app's missions, never a free-text id.
      await tester.tap(find.text('No mission'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('#9 — Operation Slipperyskin').last);
      await tester.pumpAndSettle();

      await tester.tap(find.widgetWithText(FilledButton, 'Check it out'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/equipment/item/1/checkout'));
      expect(body?['mission_id'], 9);
      expect(body?['condition'], 'good');
      expect(body?.containsKey('checked_out_by'), isFalse);
    });

    testWidgets('a checkout refusal is the server\'s words, not a hidden error',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          if (path == '/api/missions/missions') {
            return jsonResponse({'missions': <Map<String, dynamic>>[]});
          }
          if (path == '/api/equipment/item/1/checkout') {
            return jsonResponse({
              'error': 'item 1 is already out to u2 — bring it back before it '
                  'goes out again',
            }, 409);
          }
          if (path == '/api/equipment/item/1') {
            return jsonResponse({
              'item': equipmentItemFixture(),
              'open_checkout': null,
              'checkouts': <Map<String, dynamic>>[],
              'flags': <String, dynamic>{},
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      await tester.tap(find.text('Check out'));
      await tester.pumpAndSettle();

      await tester.tap(find.widgetWithText(FilledButton, 'Check it out'));
      await tester.pumpAndSettle();

      // The 409 is the answer: the server's message is kept, not replaced.
      expect(find.text('The server refused'), findsOneWidget);
      expect(
        find.textContaining('already out to u2'),
        findsOneWidget,
      );
      // The sheet stays open so the scout can see why and adjust.
      expect(find.text('Check it out'), findsWidgets);
    });

    testWidgets('an item out to me can be checked back in with a grade',
        (tester) async {
      final calls = <String>[];
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          calls.add('${request.method} $path');
          if (path == '/api/missions/missions') {
            return jsonResponse({'missions': <Map<String, dynamic>>[]});
          }
          if (path == '/api/equipment/item/1/checkin') {
            body = jsonDecode(request.body) as Map<String, dynamic>;
            return jsonResponse({'checkout': {}, 'item': {}, 'notes': 'ok'});
          }
          if (path == '/api/equipment/item/1') {
            return jsonResponse({
              'item': equipmentItemFixture(),
              'open_checkout': openCheckoutFixture(itemId: 1),
              'checkouts': <Map<String, dynamic>>[],
              'flags': <String, dynamic>{},
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      // The open checkout is visible, with who holds it and when it is due.
      expect(find.text('Checked out'), findsOneWidget);
      expect(find.textContaining('HELD BY'), findsOneWidget);

      await tester.tap(find.text('Check in'));
      await tester.pumpAndSettle();

      // The grade is chosen from the server's vocabulary; the sheet starts at
      // the item's current grade.
      await tester.tap(find.text('Good'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Fair').last);
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(FilledButton, 'Check it in'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/equipment/item/1/checkin'));
      expect(body?['condition'], 'fair');
    });
  });

  // -------------------------------------------------------------------------
  // Governance (SPEC §7.4, Accords Art 5/9/12/17)
  // -------------------------------------------------------------------------

  group('Governance API', () {
    test('the motion list sends only the filters it was given', () async {
      final calls = <Uri>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add(request.url);
          return jsonResponse({'motions': <Map<String, dynamic>>[]});
        }),
      );

      await client.motions();
      expect(calls.single.path, '/api/governance/motions');
      expect(calls.single.query, isEmpty);

      await client.motions(stage: 'voting', limit: 10);
      expect(calls.last.queryParameters, {'stage': 'voting', 'limit': '10'});
    });

    test('one motion is read by id, and the whole page comes back', () async {
      final calls = <Uri>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          calls.add(request.url);
          return jsonResponse(motionDetailFixture(
              quorum: {
                'meeting_id': 3,
                'required': 6,
                'present': 5,
                'met': false,
              },
            ));
        }),
      );

      final page = await client.motion('7');
      expect(calls.single.path, '/api/governance/motion/7');
      expect((page['motion'] as Map)['id'], 7);
      expect((page['tally'] as Map)['yes'], 3);
      expect(page['votes'], isA<List>());
      expect(page['quorum'], isA<Map>());
    });

    test('a vote posts the documented body — choice, method, note — and no more',
        () async {
      Map<String, dynamic>? body;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          expect(request.method, 'POST');
          expect(request.url.path, '/api/governance/motion/7/vote');
          body = jsonDecode(request.body) as Map<String, dynamic>;
          return jsonResponse({
            'vote_id': 9,
            'motion_id': 7,
            'choice': 'no',
            'method': 'ballot',
          }, 201);
        }),
      );

      final answer = await client.castVote('7',
          choice: 'no', method: 'ballot', note: '  reading the ledger  ');
      // The documented body names the choice and the method, and a blank note
      // is left out rather than sent empty.
      expect(body, {'choice': 'no', 'method': 'ballot', 'note': 'reading the ledger'});
      expect(answer['choice'], 'no');

      await client.castVote('7', choice: 'yes');
      expect(body, {'choice': 'yes'});
    });
  });

  group('GovernanceScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      tester.view.physicalSize = const Size(1000, 2400);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const GovernanceScreen(),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('lists each motion with its stage, its result and its tally',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          if (request.url.path == '/api/governance/motions') {
            return jsonResponse({
              'motions': [
                motionFixture(
                  id: 7,
                  title: 'Adopt the 2027 dues schedule',
                  stage: 'voting',
                  yes: 3,
                  no: 1,
                  abstain: 1,
                ),
                motionFixture(
                  id: 6,
                  title: 'Add a fourth lodge',
                  stage: 'decided',
                  result: 'failed',
                  yes: 1,
                  no: 4,
                ),
              ],
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      expect(find.text('Adopt the 2027 dues schedule'), findsOneWidget);
      // The stage is a word, not a colour — and the list carries the tally, so
      // a scout sees where a motion stands without opening it.
      expect(find.widgetWithText(StatusBadge, 'Voting'), findsOneWidget);
      expect(find.text('Yes 3 · No 1 · Abstain 1'), findsOneWidget);

      expect(find.text('Add a fourth lodge'), findsOneWidget);
      expect(find.widgetWithText(StatusBadge, 'Decided'), findsOneWidget);
      // A decided motion states its outcome, in the server's own word.
      expect(find.widgetWithText(StatusBadge, 'Failed'), findsOneWidget);
      expect(find.text('Yes 1 · No 4 · Abstain 1'), findsOneWidget);
    });

    testWidgets('a refusal reads as the permission it needs, not an empty list',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async => jsonResponse(
            {'error': 'missing permission governance:read'}, 403)),
      );

      await pump(tester, client);

      expect(find.text('The motions are not yours to read'), findsOneWidget);
      expect(find.textContaining('governance:read'), findsWidgets);
      expect(find.textContaining('missing permission governance:read'),
          findsOneWidget);
    });
  });

  group('MotionDetailScreen', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    Future<void> pump(WidgetTester tester, ApiClient client) async {
      tester.view.physicalSize = const Size(1000, 2600);
      tester.view.devicePixelRatio = 1.0;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: MaterialApp(
            theme: AppTheme.light(),
            home: const MotionDetailScreen(id: '7'),
          ),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('shows the tally, the quorum, and the caller\'s own vote',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async => jsonResponse(
              motionDetailFixture(
                quorum: {
                  'meeting_id': 3,
                  'required': 6,
                  'present': 5,
                  'met': false,
                },
                votes: [
                  {
                    'id': 1,
                    'motion_id': 7,
                    'voter': 'u2',
                    'choice': 'no',
                    'method': 'voice',
                    'recorded_at': '2026-09-26T18:00:00Z',
                    'note': '',
                  },
                  {
                    'id': 2,
                    'motion_id': 7,
                    'voter': 'u1',
                    'choice': 'yes',
                    'method': 'ballot',
                    'recorded_at': '2026-09-26T18:05:00Z',
                    'note': 'the treasurer\'s numbers convinced me',
                  },
                ],
              ),
            )),
      );

      await pump(tester, client);

      // Every figure on this screen is the server's own tally.
      expect(find.text('YES'), findsOneWidget);
      expect(find.text('NO'), findsOneWidget);
      expect(find.text('ABSTAIN'), findsOneWidget);
      expect(find.text('CAST'), findsOneWidget);
      expect(find.text('3'), findsOneWidget);
      expect(find.text('4'), findsOneWidget);
      expect(
        find.text('This would carry now, on the two thirds.'),
        findsOneWidget,
      );
      // Quorum is stated as the numbers, and what they are short by.
      expect(find.text('5 present of 6 required — short by 1.'), findsOneWidget);
      expect(find.widgetWithText(StatusBadge, 'Not met'), findsOneWidget);
      // The caller's own vote, told from the troop's by its voter.
      expect(find.text('You voted Yes (Ballot).'), findsOneWidget);
      expect(find.textContaining('the treasurer\'s numbers convinced me'),
          findsOneWidget);
    });

    testWidgets('casting a vote posts the documented body, and the screen '
        'reflects the server\'s answer', (tester) async {
      final calls = <String>[];
      Map<String, dynamic>? body;
      var voted = false;
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          calls.add('${request.method} $path');
          if (path == '/api/governance/motion/7/vote') {
            body = jsonDecode(request.body) as Map<String, dynamic>;
            voted = true;
            return jsonResponse({
              'vote_id': 9,
              'motion_id': 7,
              'choice': 'no',
              'method': 'voice',
            }, 201);
          }
          if (path == '/api/governance/motion/7') {
            return jsonResponse(motionDetailFixture(
              // After the vote the server's own page carries it — the screen
              // reads that, rather than editing its own tally.
              votes: voted
                  ? [
                      {
                        'id': 9,
                        'motion_id': 7,
                        'voter': 'u1',
                        'choice': 'no',
                        'method': 'voice',
                        'recorded_at': '2026-09-26T18:10:00Z',
                        'note': '',
                      },
                    ]
                  : <Map<String, dynamic>>[],
            ));
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      expect(find.text('You have not voted on this motion.'), findsOneWidget);

      await tester.tap(find.text('Cast vote'));
      await tester.pumpAndSettle();

      // The choice comes from the route's three, by name.
      await tester.tap(find.text('No'));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(FilledButton, 'Cast my vote'));
      await tester.pumpAndSettle();

      expect(calls, contains('POST /api/governance/motion/7/vote'));
      expect(body?['choice'], 'no');
      expect(body?['method'], 'voice');
      // The screen now shows the vote the server recorded.
      expect(find.text('You voted No (Voice).'), findsOneWidget);
      expect(find.text('Your vote was recorded'), findsOneWidget);
    });

    testWidgets('a refused vote is the server\'s words, and leaves the state '
        'unchanged', (tester) async {
      final calls = <String>[];
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          calls.add('${request.method} $path');
          if (path == '/api/governance/motion/7/vote') {
            return jsonResponse(
                {'error': 'you have already voted on this motion'}, 409);
          }
          if (path == '/api/governance/motion/7') {
            return jsonResponse(motionDetailFixture());
          }
          return http.Response('[]', 200);
        }),
      );

      await pump(tester, client);

      await tester.tap(find.text('Cast vote'));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(FilledButton, 'Cast my vote'));
      await tester.pumpAndSettle();

      // The 409 is the answer: the server's message is kept, not replaced.
      expect(find.text('The server refused'), findsOneWidget);
      expect(find.textContaining('already voted on this motion'), findsOneWidget);
      // The sheet stays open so the scout can see why, and nothing was
      // re-read or re-counted: the record is unchanged.
      expect(find.text('Cast my vote'), findsOneWidget);
      expect(find.text('You have not voted on this motion.'), findsOneWidget);
      expect(calls.where((c) => c == 'GET /api/governance/motion/7').length, 1);
    });

    testWidgets('a motion past its voteable stages offers no vote to cast',
        (tester) async {
      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async => jsonResponse(
              motionDetailFixture(
                stage: 'decided',
                result: 'passed',
                votes: [
                  {
                    'id': 1,
                    'motion_id': 7,
                    'voter': 'u1',
                    'choice': 'yes',
                    'method': 'show_of_hands',
                    'recorded_at': '2026-09-26T18:00:00Z',
                    'note': '',
                  },
                ],
              ),
            )),
      );

      await pump(tester, client);

      expect(find.widgetWithText(StatusBadge, 'Passed'), findsOneWidget);
      expect(find.text('You voted Yes (Show of hands).'), findsOneWidget);
      expect(find.text('Cast vote'), findsNothing);
    });
  });

  group('Settings → Governance', () {
    setUp(() => SharedPreferences.setMockInitialValues({}));

    testWidgets('a scout reaches the motions without curl, and the bar keeps '
        'its eight destinations', (tester) async {
      tester.view.physicalSize = const Size(1170, 2532);
      tester.view.devicePixelRatio = 3.0;
      addTearDown(tester.view.reset);

      final client = ApiClient(
        baseUrl: 'http://example.test',
        httpClient: MockClient((request) async {
          final path = request.url.path;
          if (path == '/api/announcements/unread') {
            return jsonResponse({
              'member_id': 'u1',
              'unread': 0,
              'has_urgent': false,
              'addressed': {'troop': true, 'lodges': <String>[]},
            });
          }
          if (path == '/api/governance/motions') {
            return jsonResponse({
              'motions': [
                motionFixture(id: 7, title: 'Adopt the 2027 dues schedule'),
              ],
            });
          }
          return http.Response('[]', 200);
        }),
      );

      await tester.pumpWidget(
        ChangeNotifierProvider<SessionState>(
          create: (_) => SessionState(client: client)
            ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']},
          child: const MaterialApp(home: HomeShell()),
        ),
      );
      await tester.pumpAndSettle();

      // Governance is not a ninth destination: the bottom bar is already eight
      // entries wide, and a ninth would push each below the 48dp outdoor touch
      // floor (390dp / 9 < 48). It is reached from Settings, the way Dues is.
      expect(tester.widgetList(find.byType(NavigationDestination)).length, 8);

      await tester.tap(find.text('Settings'));
      await tester.pumpAndSettle();

      expect(find.text('Governance'), findsOneWidget);
      await tester.tap(find.text('Governance'));
      await tester.pumpAndSettle();

      expect(find.text('Adopt the 2027 dues schedule'), findsOneWidget);
      expect(find.widgetWithText(StatusBadge, 'Voting'), findsOneWidget);
    });
  });
}
