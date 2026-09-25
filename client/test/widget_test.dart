import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/announcement_detail_screen.dart';
import 'package:adjutant_client/screens/announcements_screen.dart';
import 'package:adjutant_client/screens/dues_screen.dart';
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
      expect(find.text('Assessed'), findsOneWidget);
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

    testWidgets('paying is stated, not faked', (tester) async {
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

      // The one button on the screen is the tier chooser — nothing here offers
      // to take money.
      expect(find.widgetWithText(FilledButton, 'Change my tier'), findsOneWidget);
      expect(find.text('Pay dues'), findsNothing);
      expect(find.text('Pay now'), findsNothing);

      // The placeholder says where payment will live and why it is not here. It
      // is below the fold, so the page is scrolled to it.
      await tester.scrollUntilVisible(
        find.text('Paying'),
        300,
        scrollable: find.byType(Scrollable).first,
      );
      expect(find.text('Paying'), findsOneWidget);
      expect(find.textContaining('Not available yet'), findsOneWidget);
      expect(find.textContaining('card details are asked for'), findsOneWidget);
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
      expect(find.text('Settings'), findsOneWidget);
      // Seven destinations rather than the five the design language prefers, so
      // each one is still a 48dp-wide target on a 390dp phone: that floor is
      // the one that matters outdoors, and it is what makes the extra items
      // affordable rather than cramped. The shop joined it for the same reason
      // the inbox did — it is ordinary troop work, not a setting.
      final destinations =
          tester.widgetList(find.byType(NavigationDestination)).length;
      expect(destinations, 7);
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
}
