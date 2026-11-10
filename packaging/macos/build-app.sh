#!/usr/bin/env bash
# Assembles an unsigned Inlet.app from a release binary.
#
#   packaging/macos/build-app.sh <path to the inlet binary> <output directory>
#
# The bundle gets the binary, the app icon, Info.plist (version from the
# workspace Cargo.toml), LICENSE and THIRD-PARTY-NOTICES. Signing and
# notarisation happen later, in .github/workflows/macos-sign.yml.
#
# Runs with the bash 3.2 that macOS ships.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <inlet binary> <output directory>" >&2
  exit 2
fi

binary=$1
out_dir=$2
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ ! -f "$binary" ]; then
  echo "error: binary not found: $binary" >&2
  exit 1
fi

# The version comes from [workspace.package] in the root Cargo.toml.
version=$(awk '
  /^\[/ { in_section = ($0 == "[workspace.package]") ; next }
  in_section && $1 == "version" {
    sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
  }
' "$root/Cargo.toml")
if [ -z "$version" ]; then
  echo "error: no version in [workspace.package] of $root/Cargo.toml" >&2
  exit 1
fi

# CFBundleShortVersionString and CFBundleVersion take up to three integers, so
# a pre-release or build suffix (0.1.0-beta.1, 0.1.0+abc) is dropped here.
numeric=${version%%[-+]*}
if ! printf '%s\n' "$numeric" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "error: version '$version' does not start with MAJOR.MINOR.PATCH" >&2
  exit 1
fi

for f in "$here/Info.plist" "$root/assets/brand/macos/inlet.icns" "$root/LICENSE" "$root/THIRD-PARTY-NOTICES"; do
  if [ ! -f "$f" ]; then
    echo "error: missing $f" >&2
    exit 1
  fi
done

app="$out_dir/Inlet.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$binary" "$app/Contents/MacOS/inlet"
chmod 755 "$app/Contents/MacOS/inlet"

sed -e "s/@SHORT_VERSION@/$numeric/g" -e "s/@BUNDLE_VERSION@/$numeric/g" \
  "$here/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

cp "$root/assets/brand/macos/inlet.icns" "$app/Contents/Resources/inlet.icns"
cp "$root/LICENSE" "$app/Contents/Resources/LICENSE"
cp "$root/THIRD-PARTY-NOTICES" "$app/Contents/Resources/THIRD-PARTY-NOTICES"

if grep -q '@[A-Z_]*@' "$app/Contents/Info.plist"; then
  echo "error: unfilled placeholder in Info.plist" >&2
  exit 1
fi

# Checks that only work on macOS.
if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$app/Contents/Info.plist" "$here/entitlements.plist"
fi
if command -v lipo >/dev/null 2>&1; then
  archs=$(lipo -archs "$app/Contents/MacOS/inlet")
  case " $archs " in
    *" arm64 "*) ;;
    *) echo "error: the binary has no arm64 slice (found: $archs)" >&2; exit 1 ;;
  esac
fi

echo "Built $app (version $version, bundle version $numeric)"
