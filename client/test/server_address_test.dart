import 'package:adjutant_client/api/server_address.dart';
import 'package:flutter_test/flutter_test.dart';

/// The Server field takes whatever a person types, and the client used to hand
/// that string straight to `Uri.parse('$baseUrl$path')`. A bare host produced a
/// *relative* URI, so the request never left the device and the failure read as
/// "cannot reach the server" — which points at the address when the real problem
/// is the missing scheme. These pin the repair and the refusals.
void main() {
  group('normaliseServerAddress', () {
    test('fills in https for a bare host', () {
      expect(normaliseServerAddress('adjutant.example.org'),
          'https://adjutant.example.org');
    });

    test('keeps a scheme the user supplied', () {
      expect(normaliseServerAddress('http://192.168.1.20:8787'),
          'http://192.168.1.20:8787');
      expect(normaliseServerAddress('https://lodge.example.org'),
          'https://lodge.example.org');
    });

    test('strips trailing slashes so the joined request URL stays well-formed',
        () {
      expect(normaliseServerAddress('https://lodge.example.org/'),
          'https://lodge.example.org');
      expect(normaliseServerAddress('https://lodge.example.org///'),
          'https://lodge.example.org');
    });

    test('trims the whitespace a paste brings with it', () {
      expect(normaliseServerAddress('  https://lodge.example.org  '),
          'https://lodge.example.org');
    });

    test('preserves a port', () {
      expect(normaliseServerAddress('lodge.local:8787'),
          'https://lodge.local:8787');
    });

    test('preserves a path rather than silently pointing at a different box', () {
      expect(normaliseServerAddress('https://example.org/adjutant'),
          'https://example.org/adjutant');
    });

    test('refuses what is not an address', () {
      for (final junk in ['', '   ', 'not an address', 'ftp://example.org',
        'https://', '://example.org']) {
        expect(normaliseServerAddress(junk), isNull, reason: 'accepted "$junk"');
      }
    });
  });

  group('serverAddressProblem', () {
    test('says the field is required when it is empty', () {
      expect(serverAddressProblem(''), 'Enter the server address');
      expect(serverAddressProblem('   '), 'Enter the server address');
    });

    test('explains a malformed address with an example', () {
      final problem = serverAddressProblem('not an address');
      expect(problem, isNotNull);
      expect(problem, contains('https://'));
    });

    test('accepts https in every build', () {
      expect(serverAddressProblem('https://lodge.example.org'), isNull);
      expect(serverAddressProblem('lodge.example.org'), isNull);
    });

    test('a bare host is repaired to https, so it is never the blocked case', () {
      // The user who types `192.168.1.20:8787` gets https, not a refusal: the
      // refusal below is for someone who deliberately wrote http://.
      expect(serverAddressProblem('192.168.1.20:8787'), isNull);
      expect(normaliseServerAddress('192.168.1.20:8787'),
          'https://192.168.1.20:8787');
    });

    test('plain http is refused in a release build, and allowed in debug', () {
      final problem = serverAddressProblem('http://192.168.1.20:8787');
      if (cleartextAllowed) {
        expect(problem, isNull,
            reason: 'debug builds may reach a LAN box over http');
      } else {
        expect(problem, isNotNull,
            reason: 'a release build cannot reach cleartext, so it must say so '
                'rather than fail as an unreachable server');
        expect(problem, contains('https'));
      }
    });
  });
}
