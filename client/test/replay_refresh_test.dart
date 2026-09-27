/// Issue #124: after a queued write replays, the screens must re-read.
///
/// Observed on a real device: with the server down a receipt was queued
/// ("Offline — your receipt is queued and will sync."), the server came back,
/// the app bar's retry reported "1 sent." — and the Inbox still said "1 unread
/// of 1", the row still offered "Mark read", and the destination badge still
/// showed 1, until the destination was re-entered. The server was right the
/// whole time.
///
/// This drives the shell the way the person did — a mock `http.Client` toggled
/// from unreachable to reachable inside its own handler — and asserts on what
/// is *on screen* after the replay, not on what the client intended. It fails
/// against a shell that does not re-read: the row keeps offering "Mark read".
library;

import 'dart:convert';

import 'package:adjutant_client/api/api_client.dart';
import 'package:adjutant_client/screens/home_shell.dart';
import 'package:adjutant_client/state/session.dart';
import 'package:adjutant_client/theme/app_theme.dart';
import 'package:adjutant_client/widgets/common.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:provider/provider.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// A JSON response the way the server sends one: UTF-8 bytes.
http.Response jsonResponse(Object body, [int status = 200]) =>
    http.Response.bytes(
      utf8.encode(jsonEncode(body)),
      status,
      headers: {'content-type': 'application/json; charset=utf-8'},
    );

void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  testWidgets(
      'a replay that sends a write makes the inbox re-read the server state',
      (tester) async {
    // A phone: the width the bug was seen on, and the compact layout.
    tester.view.physicalSize = const Size(1170, 2532);
    tester.view.devicePixelRatio = 3.0;
    addTearDown(tester.view.reset);

    // The inbox as the device holds it: one unread notice, and the badge the
    // server last gave. This is what the screens open onto while offline.
    SharedPreferences.setMockInitialValues({
      'cache.announcements': jsonEncode([
        {
          'id': 7,
          'title': 'Meeting moved to Thursday',
          'category': 'urgent',
          'scope_type': 'troop',
          'status': 'published',
          'is_read': false,
        },
      ]),
      'cache.announcements.at': DateTime(2026, 9, 27, 7, 30).toIso8601String(),
      'cache.announcement.badge': jsonEncode({
        'member_id': 'u1',
        'visible': 1,
        'unread': 1,
        'read': 0,
        'urgent_unread': 1,
        'has_urgent': true,
      }),
      'cache.announcement.badge.at':
          DateTime(2026, 9, 27, 7, 30).toIso8601String(),
    });

    // One transport fake, toggled mid-test: offline first, then the server is
    // back. `up` is read inside the handler and flipped below — offline-then-
    // online without rebuilding the client.
    var up = false;
    // The server's own record of the receipt. The read route is what changes
    // it, exactly as the server writes the receipt on that route.
    var read = false;
    final sent = <String>[];

    final client = ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((request) async {
        sent.add('${request.method} ${request.url.path}');
        if (!up) throw http.ClientException('no route to host');
        final path = request.url.path;

        if (path == '/api/announcements/announcement/7/read') {
          // The server records the receipt. Every later read reflects it.
          read = true;
          return jsonResponse({'is_read': true, 'unread': _badge(read, 1)});
        }
        if (path == '/api/announcements/unread') {
          return jsonResponse(_badge(read, 1));
        }
        if (path == '/api/announcements/announcements') {
          return jsonResponse({
            'announcements': [
              {
                'id': 7,
                'title': 'Meeting moved to Thursday',
                'category': 'urgent',
                'scope_type': 'troop',
                'status': 'published',
                'is_read': read,
              },
            ],
            'count': 1,
          });
        }
        // Everything else the shell's first destination asks for, empty.
        return http.Response('[]', 200);
      }),
    );

    final session = SessionState(client: client)
      ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']};

    await tester.pumpWidget(
      ChangeNotifierProvider<SessionState>.value(
        value: session,
        child: MaterialApp(theme: AppTheme.light(), home: const HomeShell()),
      ),
    );
    await tester.pumpAndSettle();

    // Open the inbox. Offline, so it is the cached copy on screen.
    await tester.tap(find.text('Inbox'));
    await tester.pumpAndSettle();

    expect(find.text('Meeting moved to Thursday'), findsOneWidget);
    expect(find.text('1 unread of 1 in this inbox'), findsOneWidget);
    expect(find.text('Mark read'), findsOneWidget);

    // Mark it read with the server down: a receipt is recorded on the device
    // rather than pretending it reached the server, and the row is not flipped.
    await tester.tap(find.text('Mark read'));
    await tester.pumpAndSettle();

    expect(session.pendingWriteCount, 1);
    expect(session.pendingWrites.single.path,
        '/api/announcements/announcement/7/read');
    expect(find.textContaining('queued and will sync'), findsOneWidget);
    expect(find.text('Mark read'), findsOneWidget,
        reason: 'a queued write is not applied to the row it belongs to');

    // The server comes back, and the person presses the app bar's retry.
    up = true;
    await tester.tap(find.byIcon(Icons.cloud_upload_outlined));
    await tester.pumpAndSettle();

    // The write went: the server was told to record the receipt.
    expect(session.pendingWriteCount, 0);
    expect(sent, contains('POST /api/announcements/announcement/7/read'));

    // And the screens re-read rather than showing the state from before the
    // write left: the inbox now shows the server's new answer, and the row
    // offers to undo the receipt instead of repeating it.
    expect(find.text('0 unread of 1 in this inbox'), findsOneWidget);
    expect(find.text('Mark unread'), findsOneWidget);
    expect(find.text('Mark read'), findsNothing);
    // The destination badge followed: no unread, so no badge over the inbox.
    expect(find.text('1'), findsNothing);

    // The re-read was a live one, not a cached copy dressed up: the server was
    // reachable, so the offline banner is gone.
    expect(find.byType(OfflineBanner), findsNothing);
  });

  testWidgets('a replay that sends nothing does not disturb the screen',
      (tester) async {
    tester.view.physicalSize = const Size(1170, 2532);
    tester.view.devicePixelRatio = 3.0;
    addTearDown(tester.view.reset);

    SharedPreferences.setMockInitialValues({
      'cache.announcements': jsonEncode([
        {
          'id': 7,
          'title': 'Meeting moved to Thursday',
          'category': 'urgent',
          'scope_type': 'troop',
          'status': 'published',
          'is_read': false,
        },
      ]),
      'cache.announcements.at': DateTime(2026, 9, 27, 7, 30).toIso8601String(),
      'cache.announcement.badge': jsonEncode({
        'member_id': 'u1',
        'visible': 1,
        'unread': 1,
        'read': 0,
        'urgent_unread': 1,
        'has_urgent': true,
      }),
      'cache.announcement.badge.at':
          DateTime(2026, 9, 27, 7, 30).toIso8601String(),
    });

    // The server never comes back, so the retry sends nothing.
    final attempted = <String>[];
    final client = ApiClient(
      baseUrl: 'http://example.test',
      httpClient: MockClient((request) async {
        attempted.add('${request.method} ${request.url.path}');
        throw http.ClientException('no route to host');
      }),
    );
    final session = SessionState(client: client)
      ..user = {'id': 'u1', 'username': 'scout', 'roles': ['scout']};

    await tester.pumpWidget(
      ChangeNotifierProvider<SessionState>.value(
        value: session,
        child: MaterialApp(theme: AppTheme.light(), home: const HomeShell()),
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('Inbox'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Mark read'));
    await tester.pumpAndSettle();

    final epochBefore = session.reloadEpoch;
    attempted.clear();

    await tester.tap(find.byIcon(Icons.cloud_upload_outlined));
    await tester.pumpAndSettle();

    // The retry did run a pass — the POST was attempted...
    expect(attempted, contains('POST /api/announcements/announcement/7/read'));
    // ...and it was unreachable, so nothing changed: no generation bump, and
    // the queue still holds the receipt.
    expect(session.reloadEpoch, epochBefore);
    expect(session.pendingWriteCount, 1);
  });
}

/// The server's unread badge, moved by whether the receipt has been recorded.
Map<String, dynamic> _badge(bool read, int visible) => {
      'member_id': 'u1',
      'visible': visible,
      'unread': read ? 0 : 1,
      'read': read ? 1 : 0,
      'urgent_unread': read ? 0 : 1,
      'has_urgent': !read,
    };
