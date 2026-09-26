/// What a person types into the "Server" field, turned into one base URL.
///
/// The field asks for "the address of your troop's Adjutant server", and the
/// answers arrive as `adjutant.example.org`, `https://adjutant.example.org/`,
/// `192.168.1.20:8787`. So the client has to choose the scheme, drop trailing
/// slashes, and say something useful when the address cannot work — because the
/// alternative is a relative `Uri` or a request the operating system refuses,
/// both of which surface as "cannot reach the server" and send the user to debug
/// the address when the problem is actually the scheme.
library;

import 'package:flutter/foundation.dart' show kDebugMode;

/// Whether this build can reach a plain-`http` server.
///
/// Android has refused cleartext by default since API 28, and this app relaxes
/// that for **debug builds only** — see
/// `android/app/src/debug/res/xml/network_security_config.xml`. The answer is
/// therefore a property of the build rather than of the network, and stating it
/// in one place keeps the UI from guessing at it.
///
/// A release build requires `https://`. That is not a limitation invented here:
/// every option in `docs/deployment.md` § Putting it behind TLS serves https
/// (a public name with ACME, a tunnel, or a private CA on the LAN), so a release
/// build that accepted `http://` would be accepting an address the server does
/// not hand out and credentials the network can read.
const bool cleartextAllowed = kDebugMode;

/// The canonical base URL for [raw] — scheme filled in, trailing slashes gone —
/// or null when [raw] cannot be made usable. Callers report
/// [serverAddressProblem] rather than guessing at why.
String? normaliseServerAddress(String raw) {
  final trimmed = raw.trim();
  if (trimmed.isEmpty) return null;
  // An address with a space in it is a typo, not a hostname.
  if (trimmed.contains(RegExp(r'\s'))) return null;

  // A bare host is the common answer. `https` because it is the one that works
  // in a release build and the only one any documented deployment serves.
  final withScheme = trimmed.contains('://') ? trimmed : 'https://$trimmed';

  final uri = Uri.tryParse(withScheme);
  if (uri == null) return null;
  if (uri.scheme != 'http' && uri.scheme != 'https') return null;
  if (uri.host.isEmpty) return null;

  // Strip trailing slashes so `$baseUrl$path` stays well-formed. A path is
  // otherwise preserved rather than silently discarded, so an address that
  // carries one is not quietly rewritten into a different server.
  var out = withScheme;
  while (out.endsWith('/')) {
    out = out.substring(0, out.length - 1);
  }
  return out;
}

/// Why [raw] cannot be used, in words the person who typed it can act on — or
/// null when it is fine.
String? serverAddressProblem(String raw) {
  final trimmed = raw.trim();
  if (trimmed.isEmpty) return 'Enter the server address';

  final normalised = normaliseServerAddress(trimmed);
  if (normalised == null) {
    return 'That does not look like an address. Try something like '
        'https://adjutant.your-troop.org';
  }
  if (normalised.startsWith('http://') && !cleartextAllowed) {
    return 'This app reaches a server over https only, and the operating '
        'system blocks plain http:// outright. Enter the https address — ask '
        'whoever set up your troop\'s server if you do not have it.';
  }
  return null;
}
