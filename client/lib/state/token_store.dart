/// Where the session bearer token lives.
///
/// A 30-day bearer token is a credential, not a preference. It used to sit in
/// `shared_preferences` beside the server address and the read cache — plaintext
/// on disk, included in Android Auto Backup unless excluded — so the storage is
/// now an injected interface with one real implementation and one for tests.
///
/// **Tests must not touch a platform channel.** `flutter test` has no Keystore,
/// no Keychain and no libsecret; [InMemoryTokenStore] is what the suite injects,
/// so `SessionState.boot()`/`signIn()`/`signOut()` are exercised as ordinary
/// Dart.
///
/// **Web is the honest exception.** On the web, `flutter_secure_storage` writes
/// to `localStorage` — there is no OS keychain in a browser. That is the only
/// store available to a pure web build, and `README.md` says so rather than
/// claiming the browser token is encrypted. The Android exclusion in the
/// manifest and the Keystore encryption are what the native builds gain.
library;

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// The storage contract the session depends on.
abstract class TokenStore {
  Future<String?> read();
  Future<void> write(String token);
  Future<void> delete();
}

/// Keystore (Android) / Keychain (iOS, macOS) / libsecret (Linux) storage.
///
/// The key name is not the old `adjutant.token` preference key: the token lives
/// in a different store now, and keeping the old name would make a migration
/// look like it had already happened.
class SecureTokenStore implements TokenStore {
  const SecureTokenStore([this._storage = const FlutterSecureStorage()]);

  /// The key inside the secure store. Not the `shared_preferences` key.
  static const String storageKey = 'adjutant.session.token';

  final FlutterSecureStorage _storage;

  @override
  Future<String?> read() => _storage.read(key: storageKey);

  @override
  Future<void> write(String token) =>
      _storage.write(key: storageKey, value: token);

  @override
  Future<void> delete() => _storage.delete(key: storageKey);
}

/// A store that keeps the token in this process only.
///
/// For tests, and for a platform with no secure storage at all. It is not a
/// fallback used silently in the app: [SessionState] takes the store it is
/// given, and production gives it [SecureTokenStore].
class InMemoryTokenStore implements TokenStore {
  InMemoryTokenStore([this._token]);

  String? _token;

  /// The token currently held, for assertions in tests.
  String? get token => _token;

  @override
  Future<String?> read() async => _token;

  @override
  Future<void> write(String token) async => _token = token;

  @override
  Future<void> delete() async => _token = null;
}
