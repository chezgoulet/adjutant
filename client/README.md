# Adjutant Client

The Flutter client for Adjutant — one codebase for Android, iOS, web, and desktop.

## What this is

A **renderer for server state**. The client holds no authority of its own: every
action is an ordinary API call subject to the server's route gate, and hiding a
control is convenience, never security
([`docs/design/client-and-plugin-ui.md`](../docs/design/client-and-plugin-ui.md) §2).

Two rules the code follows:

1. **Offline is a normal state, not an error.** Scouts are in the woods. A screen
   that shows the last-known truth beats a screen that shows a spinner. Every
   list read is cached and served stale-with-a-banner when the network is gone.
2. **Token discipline.** No widget invents a colour, size, or text style. All
   visual values come from [`lib/theme/app_theme.dart`](lib/theme/app_theme.dart),
   which implements [`docs/design/flutter-design-language.md`](../docs/design/flutter-design-language.md).

## Layout

```
lib/
  main.dart                      app entry, theme wiring, auth routing
  api/api_client.dart            the one place the client talks to the server
  state/session.dart             session, token, offline cache
  theme/app_theme.dart           design tokens (colour, type, spacing, radius)
  widgets/common.dart            shared vocabulary: StatusBadge, AppCard,
                                 EmptyState, OfflineBanner, DetailField, formatters
  screens/
    login_screen.dart            sign in + point at your server
    home_shell.dart              adaptive shell: bottom bar / rail / sidebar
    dashboard_screen.dart        Monitor surface
    missions_screen.dart         the six-stage lifecycle, list + detail
    calendar_screen.dart         month grid, upcoming, quorum visibility
    members_screen.dart          roster with search
test/widget_test.dart            unit + widget tests for the logic and legibility
```

## Platform support

Flutter compiles this one codebase to every target. What differs per platform is
the **build toolchain**, not the code:

| Target | Status | Requirement |
|---|---|---|
| **Web (PWA)** | builds on Linux | none beyond Flutter |
| **Linux desktop** | builds on Linux | `clang`, `cmake`, `ninja-build`, `libgtk-3-dev`, `pkg-config` |
| **Android** | needs toolchain | Android SDK + JDK 17 |
| **iOS** | **macOS only** | macOS + Xcode — Apple does not permit iOS builds elsewhere |
| **macOS desktop** | **macOS only** | macOS + Xcode |
| **Windows desktop** | **Windows only** | Windows + Visual Studio |

The `android/`, `ios/`, `macos/`, and `windows/` directories are committed and
configured. Building for those platforms happens on a host that can run their
toolchain — including CI. Nothing in `lib/` needs to change to produce them.

## Running

```bash
# Install dependencies
flutter pub get

# Run against a local server (any target)
flutter run -d chrome --dart-define=ADJUTANT_BASE_URL=http://localhost:8080

# Tests and analysis
flutter test
flutter analyze
```

The server address is set on the login screen and remembered, so a troop points
the app at its own box without a rebuild.

## Building

```bash
flutter build web --release --wasm       # build/web  (see the caveats below)
flutter build web --release              # build/web  (JS/CanvasKit)
flutter build linux --release        # build/linux/x64/release/bundle
flutter build apk --release          # needs Android SDK
flutter build ipa --release          # macOS only
```

### Web: WASM is a conditional win, not a free one

Flutter emits **both** `main.dart.wasm` and `main.dart.js` and picks at runtime by
detecting WasmGC support, so the app runs everywhere either way. But WASM is only
*better* in some places, and the difference is not cosmetic:

- **iOS browsers get no WASM at all.** Every browser on iOS is required to use
  WebKit, and Flutter's WASM renderer cannot run there. An iPhone user runs the
  JS build regardless. Ship WASM for desktop and Android Chromium; do not expect
  it to change anything on iOS.
- **Multi-threading needs HTTP headers, or the main win is lost.** WASM uses
  multiple threads to render faster — but only if the server sends:

  | Header | Value |
  |---|---|
  | `Cross-Origin-Embedder-Policy` | `credentialless` (or `require-corp`) |
  | `Cross-Origin-Opener-Policy` | `same-origin` |

  Without them the app still runs, single-threaded, and much of the performance
  argument evaporates. **Whoever serves this must set those headers** or the WASM
  build is a bigger download for no gain.
- **Firefox and Safari currently fall back** to JS because of known bugs in
  Flutter's WASM renderer.
- **Deferred loading is experimental** under WASM (`--enable-wasm-deferred-loading`).

Verified here: the WASM build compiles and renders the login screen identically
to the JS build. That confirms it is not broken — it does not by itself confirm
it is faster on any given deployment, which depends on the headers above.

If the headers cannot be set, build without `--wasm` and take the JS path
knowingly.

## Offline behaviour

- **Reads** always succeed: fresh from the server, or the cached copy with an
  offline banner naming the time it was cached.
- **A 401 signs you out; a network failure does not.** An unreachable server
  means "offline", not "your session is invalid".
- **Writes** are not yet queued. The outbox for queued writes is the next piece
  of offline work; today the client is read-offline only, and says so rather than
  silently dropping an action.

## Not yet built

Stated plainly so nobody assumes otherwise:

- The **plugin UI metadata renderer** (screens driven by `/api/<plugin>/_ui`).
  The contract is defined; the first-party screens here are hand-built, which the
  design doc calls workable for first-party plugins. Metadata is what lets a
  *community* plugin have a screen without a client release.
- **Queued writes** (the outbox).
- **Push notifications.**
- **Governance screens** (motions, voting, minutes) — the plugin is built and the
  API is there; the screens are not.
- **Roster editing, mission creation, event creation** — the client is read-only
  apart from RSVP.
