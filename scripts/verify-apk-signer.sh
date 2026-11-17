#!/usr/bin/env bash
# Verify that a built APK is not signed with the public Android debug key.
#
# The debug keystore is a public artefact: its alias and password are well known,
# so an APK signed with it can be replaced by anybody's build that Android will
# accept as an update. A release must print the real signer DN and fail if it
# sees `CN=Android Debug` (#168).
#
# Usage: scripts/verify-apk-signer.sh path/to/app-release.apk
set -euo pipefail

apk="${1:-}"
if [[ -z "$apk" || ! -f "$apk" ]]; then
  echo "::error::usage: $0 path/to/app-release.apk (no such file: '${apk}')" >&2
  exit 1
fi

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-${ANDROID_SDK:-}}}"
apksigner=""
if command -v apksigner >/dev/null 2>&1; then
  apksigner="$(command -v apksigner)"
elif [[ -n "$sdk" && -d "$sdk/build-tools" ]]; then
  apksigner="$(find "$sdk/build-tools" -type f -name apksigner 2>/dev/null | sort -V | tail -n 1)"
fi

if [[ -z "$apksigner" || ! -x "$apksigner" ]]; then
  echo "::error::apksigner not found. Set ANDROID_HOME/ANDROID_SDK_ROOT, or put apksigner on PATH." >&2
  exit 1
fi

report="$("$apksigner" verify --print-certs "$apk")"
printf '%s\n' "$report"

if grep -q 'CN=Android Debug' <<<"$report"; then
  echo "::error::$apk is signed with the public Android debug key (CN=Android Debug). Refusing this artifact (#168)." >&2
  exit 1
fi

if ! grep -q 'certificate DN:' <<<"$report"; then
  echo "::error::$apk has no signer certificate DN in apksigner's report; cannot prove what signed it." >&2
  exit 1
fi

echo "Signer is not the Android debug key."
