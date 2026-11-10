#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES at the repository root with cargo-about
# (https://github.com/EmbarkStudios/cargo-about), from about.toml and about.hbs.
# The file is committed (the app embeds it in its About view) and shipped next to
# every binary. Run it again after any dependency change.
#
#   cargo install cargo-about --locked --version 0.9.3 --features cli   # once
#   scripts/third-party-notices.sh
#
# cargo-about does not promise an order for licence texts that share a licence id
# (two Apache-2.0 texts can swap between runs), which made the committed file and
# the CI freshness check flaky. The template (about.hbs) marks where the licence
# blocks start and end, and this script sorts them: blocks by licence id, then by
# the first crate in the block; the crates inside a block by name; the overview by
# count, then name. The fixed sections after the blocks (fonts, icons, emoji) stay
# where they are. Two runs on the same inputs give identical bytes.
set -euo pipefail
cd "$(dirname "$0")/.."
export LC_ALL=C

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo about generate --locked --manifest-path crates/inlet-app/Cargo.toml --features demo about.hbs -o "$work/raw"

mkdir "$work/blocks"
awk -v dir="$work/blocks" '
  # Overview lines are "- <name>: <count> crate(s)": count descending, then the line.
  function ov_before(a, b,   ca, cb) {
    ca = a; sub(/ crate\(s\)$/, "", ca); sub(/^.*: /, "", ca)
    cb = b; sub(/ crate\(s\)$/, "", cb); sub(/^.*: /, "", cb)
    if (ca + 0 != cb + 0) return (ca + 0) > (cb + 0)
    return a < b
  }
  function flush_overview(   i, j, t) {
    for (i = 2; i <= nov; i++) {
      t = ov[i]
      for (j = i - 1; j >= 1 && ov_before(t, ov[j]); j--) ov[j + 1] = ov[j]
      ov[j + 1] = t
    }
    for (i = 1; i <= nov; i++) print ov[i] > (dir "/header")
    nov = 0
  }
  # Writes one licence block with its "  - crate version" lines sorted, and an index
  # line "<licence id> TAB <first crate line> TAB <block number>".
  function flush_block(   i, j, t, id, name) {
    for (i = 2; i <= nused; i++) {
      t = used[i]
      for (j = i - 1; j >= 1 && t < used[j]; j--) used[j + 1] = used[j]
      used[j + 1] = t
    }
    id = title; sub(/^.*\(/, "", id); sub(/\)$/, "", id)
    name = dir "/block-" nblocks
    printf "%s", head > name
    for (i = 1; i <= nused; i++) print used[i] > name
    printf "%s", rest > name
    close(name)
    printf "%s\t%s\t%d\n", id, used[1], nblocks > (dir "/index")
  }
  function start_block() { nblocks++; nused = 0; head = ""; rest = ""; title = ""; state = "block-top" }
  BEGIN { state = "header"; nov = 0 }
  state == "header" {
    if ($0 == "@@INLET-BLOCK@@") { flush_overview(); start_block(); next }
    if ($0 ~ /^- .*: [0-9]+ crate\(s\)$/) { ov[++nov] = $0; next }
    if (nov > 0) flush_overview()
    print > (dir "/header")
    next
  }
  state == "block-top" {
    # separator line, "<name> (<id>)", "Used by:", then the crate lines
    if ($0 ~ /^  - /) { used[++nused] = $0; state = "block-used"; next }
    head = head $0 "\n"
    if ($0 ~ /\)$/ && title == "") title = $0
    next
  }
  state == "block-used" {
    if ($0 ~ /^  - /) { used[++nused] = $0; next }
    state = "block-rest"
    rest = $0 "\n"
    next
  }
  state == "block-rest" {
    if ($0 == "@@INLET-BLOCK@@") { flush_block(); start_block(); next }
    if ($0 == "@@INLET-END@@") { flush_block(); state = "tail"; next }
    rest = rest $0 "\n"
    next
  }
  state == "tail" { print > (dir "/tail"); next }
  END {
    if (state != "tail") {
      print "third-party-notices.sh: the about.hbs markers are missing from the cargo-about output" > "/dev/stderr"
      exit 1
    }
    close(dir "/header"); close(dir "/tail"); close(dir "/index")
  }
' "$work/raw"

tab="$(printf '\t')"
{
  cat "$work/blocks/header"
  sort -t "$tab" -k1,1 -k2,2 "$work/blocks/index" | while IFS="$tab" read -r _ _ n; do
    cat "$work/blocks/block-$n"
  done
  cat "$work/blocks/tail"
} > "$work/sorted"

cat "$work/sorted" > THIRD-PARTY-NOTICES
echo "wrote THIRD-PARTY-NOTICES ($(wc -l < THIRD-PARTY-NOTICES) lines)"
