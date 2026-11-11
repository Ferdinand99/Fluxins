#!/usr/bin/env bash
# Takes the screenshots of a CI run as the new baselines of the visual checks.
#
#   scripts/update-visual-baselines.sh <run id> [windows|macos|linux ...]
#
# Use it when a change to the look is meant: open the run's artifacts visual-<os>
# first (report.md, and the red pixels in diff/) and make sure every difference is
# wanted. The script downloads those artifacts with the GitHub CLI and copies their
# actual/*.png (already cut to the window and compressed by inlet-visual) to
# tests/visual/baseline/<os>/. It commits nothing: look at the new pictures, then
# commit them with the change that caused them.
#
# Baselines always come from CI, from the same pinned runner images that compare
# them; screenshots from another machine would differ in fonts and rendering.
# The run id is in the URL of the workflow run (…/actions/runs/<id>), or:
#   gh run list --workflow build.yml --branch <branch>
set -euo pipefail
cd "$(dirname "$0")/.."

[ $# -ge 1 ] || { sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
run="$1"
shift
case "$run" in *[!0-9]*) echo "run id must be a number" >&2; exit 2 ;; esac
oses=("$@")
[ ${#oses[@]} -gt 0 ] || oses=(windows macos linux)

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
for os in "${oses[@]}"; do
  case "$os" in windows | macos | linux) ;; *) echo "unknown platform $os" >&2; exit 2 ;; esac
  if ! gh run download "$run" -n "visual-$os" -D "$tmp/$os"; then
    echo "no artifact visual-$os in run $run; skipped" >&2
    continue
  fi
  shopt -s nullglob
  pictures=("$tmp/$os"/actual/*.png)
  shopt -u nullglob
  if [ ${#pictures[@]} -eq 0 ]; then
    echo "visual-$os has no screenshots; skipped" >&2
    continue
  fi
  mkdir -p "tests/visual/baseline/$os"
  cp "${pictures[@]}" "tests/visual/baseline/$os/"
  echo "$os: ${#pictures[@]} baselines from run $run"
done
git status --short tests/visual/baseline
