#!/usr/bin/env bash
# Sends one file to Apple's notary service, waits for the verdict and staples
# the ticket to it.
#
#   packaging/macos/notarize.sh <file to submit> <file to staple>
#
# For an app bundle, submit a zip of it (ditto -c -k --keepParent) and staple
# the bundle. For a DMG, pass the DMG twice.
#
# Reads the App Store Connect API key from APPLE_API_KEY_ID,
# APPLE_API_ISSUER_ID and APPLE_API_KEY_P8_BASE64. The key is written to a
# private temporary file (mode 600) and deleted when the script exits. No
# secret is printed. Never run this with `set -x`.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <file to submit> <file to staple>" >&2
  exit 2
fi
submit=$1
staple=$2

for name in APPLE_API_KEY_ID APPLE_API_ISSUER_ID APPLE_API_KEY_P8_BASE64; do
  if [ -z "${!name:-}" ]; then
    echo "error: $name is not set" >&2
    exit 1
  fi
done

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/notary.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

key="$work/AuthKey.p8"
(
  umask 077
  printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$key"
)
chmod 600 "$key"
if [ ! -s "$key" ]; then
  echo "error: APPLE_API_KEY_P8_BASE64 did not decode to a key" >&2
  exit 1
fi

auth=(--key "$key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID")

echo "Submitting $(basename "$submit") for notarisation"
result="$work/result.json"
set +e
xcrun notarytool submit "$submit" "${auth[@]}" --wait --timeout 45m --output-format json > "$result"
submit_status=$?
set -e

id=$(plutil -extract id raw -o - "$result" 2>/dev/null || true)
status=$(plutil -extract status raw -o - "$result" 2>/dev/null || true)
echo "Submission ${id:-unknown}: ${status:-no status} (notarytool exit $submit_status)"

if [ "$status" != "Accepted" ]; then
  if [ -n "$id" ]; then
    echo "Notary log:"
    xcrun notarytool log "$id" "${auth[@]}" || true
  fi
  echo "error: notarisation of $(basename "$submit") was not accepted" >&2
  exit 1
fi

xcrun stapler staple "$staple"
xcrun stapler validate "$staple"
