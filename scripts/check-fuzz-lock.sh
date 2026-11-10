#!/usr/bin/env bash
# The fuzz target of inlet-media has its own workspace and its own Cargo.lock
# (crates/inlet-media/fuzz), so cargo-deny's run on the main lock does not see it.
# This check keeps the two in step: every crate that appears in both locks must
# have a version that the main lock also uses, so that the decoders being fuzzed are
# the decoders being shipped. Run it after any dependency change:
#
#   scripts/check-fuzz-lock.sh
#
# To fix a mismatch: cd crates/inlet-media/fuzz && cargo update -p <crate> --precise <version>
set -euo pipefail
cd "$(dirname "$0")/.."

main=Cargo.lock
fuzz=crates/inlet-media/fuzz/Cargo.lock

# "name version" for every package of a lock file.
packages() {
  awk '
    /^name = / { gsub(/"/, "", $3); name = $3 }
    /^version = / { gsub(/"/, "", $3); if (name != "") print name, $3; name = "" }
  ' "$1" | sort -u
}

packages "$main" >/tmp/fuzz-check-main.$$
packages "$fuzz" >/tmp/fuzz-check-fuzz.$$
trap 'rm -f /tmp/fuzz-check-main.$$ /tmp/fuzz-check-fuzz.$$' EXIT

fail=0
while read -r name version; do
  if grep -q "^$name " /tmp/fuzz-check-main.$$; then
    if ! grep -qx "$name $version" /tmp/fuzz-check-main.$$; then
      echo "mismatch: $name is $version in the fuzz lock, but the main lock has:" \
        "$(grep "^$name " /tmp/fuzz-check-main.$$ | cut -d' ' -f2 | paste -sd, -)" >&2
      fail=1
    fi
  fi
done </tmp/fuzz-check-fuzz.$$

# The decoders themselves must be in the fuzz lock at all.
for crate in image png gif image-webp zune-jpeg; do
  if ! grep -q "^$crate " /tmp/fuzz-check-fuzz.$$; then
    echo "the fuzz lock has no $crate; is the fuzz target still built on inlet-media?" >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "fuzz lock is out of step with the main lock (see the header of this script)" >&2
  exit 1
fi
echo "fuzz lock matches the main lock ($(grep -c . /tmp/fuzz-check-fuzz.$$) packages)"
