#!/usr/bin/env bash
# Configure Android release signing from the four Actions secrets.
#
# The real release keystore is provisioned as repository secrets; this script
# only writes the two files Gradle reads and never prints a secret value. If any
# secret is absent it fails the job loudly — a debug-signed APK must never be
# produced by the release workflow again (#168).
#
# Usage: scripts/android-release-signing.sh [client/android]
set -euo pipefail

android_dir="${1:-client/android}"

required=(
  ANDROID_KEYSTORE_BASE64
  ANDROID_KEYSTORE_PASSWORD
  ANDROID_KEY_ALIAS
  ANDROID_KEY_PASSWORD
)

missing=()
for name in "${required[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    missing+=("$name")
  fi
done

if (( ${#missing[@]} > 0 )); then
  echo "::error::Android release signing is not configured; missing: ${missing[*]}. Refusing to build — without these the APK would be signed with the public Android debug key (#168)."
  echo "A release APK must never fall back to the debug key." >&2
  exit 1
fi

if [[ ! -d "$android_dir" ]]; then
  echo "::error::$android_dir does not exist" >&2
  exit 1
fi

# Keep the key material private for the rest of the build.
umask 077

keystore="$android_dir/adjutant-release.jks"
printf '%s' "$ANDROID_KEYSTORE_BASE64" | base64 --decode > "$keystore"
keystore_abs="$(cd "$android_dir" && pwd)/adjutant-release.jks"

{
  printf 'storeFile=%s\n' "$keystore_abs"
  printf 'storePassword=%s\n' "$ANDROID_KEYSTORE_PASSWORD"
  printf 'keyAlias=%s\n' "$ANDROID_KEY_ALIAS"
  printf 'keyPassword=%s\n' "$ANDROID_KEY_PASSWORD"
} > "$android_dir/key.properties"

# Deliberately says what was written, not what it contains.
echo "Wrote $android_dir/key.properties and its keystore (contents not printed)."
