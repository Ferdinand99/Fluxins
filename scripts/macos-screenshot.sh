#!/usr/bin/env bash
# Starts the demo build of Inlet on macOS once for every scene file in
# tests/visual/scenes and saves a screenshot of its window (compared with the baselines
# by inlet-visual, which cuts the title bar off). Used by CI.
# Usage: scripts/macos-screenshot.sh <path to the inlet binary> <output folder>
#   The screenshots and logs land in <output folder>/visual.
#
# The CI runner's screen is 1024x768, too small for a 1280x860 scene window: the
# script switches it to 1920x1080 at one pixel per point (no Retina scaling) first,
# and parks the pointer in a corner so nothing shows a hover state. Needs Xcode's
# swiftc for a small helper (screen mode, pointer, window lookup).
set -uo pipefail
bin="$1"
out="$2/visual"
scenes_dir="$(cd "$(dirname "$0")/.." && pwd)/tests/visual/scenes"
mkdir -p "$out"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cat >"$tmp/helper.swift" <<'EOF'
import CoreGraphics
import Foundation

let args = CommandLine.arguments
let display = CGMainDisplayID()
switch args.count > 1 ? args[1] : "" {
case "display":
    // display <width> <height>: a mode with that size at one pixel per point.
    let w = Int(args[2])!, h = Int(args[3])!
    let modes = (CGDisplayCopyAllDisplayModes(display, nil) as? [CGDisplayMode]) ?? []
    guard let mode = modes.first(where: { $0.width == w && $0.height == h && $0.pixelWidth == w }) else {
        print("no \(w)x\(h) mode; the screen stays \(CGDisplayPixelsWide(display))x\(CGDisplayPixelsHigh(display))")
        exit(1)
    }
    var config: CGDisplayConfigRef?
    CGBeginDisplayConfiguration(&config)
    CGConfigureDisplayWithDisplayMode(config, display, mode, nil)
    let result = CGCompleteDisplayConfiguration(config, .forSession)
    sleep(2)
    print("display: \(CGDisplayPixelsWide(display))x\(CGDisplayPixelsHigh(display)) (result \(result.rawValue))")
case "pointer":
    CGWarpMouseCursorPosition(CGPoint(x: 0, y: CGFloat(CGDisplayPixelsHigh(display) - 1)))
case "window":
    // window <pid>: the ID of the process's largest normal window.
    let pid = Int(args[2])!
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    let windows = list.filter {
        ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0
    }
    let area = { (w: [String: Any]) -> Double in
        let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
        return (b["Width"] ?? 0) * (b["Height"] ?? 0)
    }
    guard let best = windows.max(by: { area($0) < area($1) }), let id = best[kCGWindowNumber as String] as? Int else {
        exit(1)
    }
    print(id)
default:
    print("usage: helper display <w> <h> | pointer | window <pid>")
    exit(2)
}
EOF
swiftc -O "$tmp/helper.swift" -o "$tmp/helper" || exit 1
"$tmp/helper" display 1920 1080 || exit 1
"$tmp/helper" pointer

failed=""
for file in "$scenes_dir"/*.toml; do
  name=$(sed -n 's/^name *= *"\([^"]*\)".*/\1/p' "$file" | head -1)
  RUST_LOG=info INLET_DEMO_SCENE="$file" "$bin" >"$out/$name.log" 2>&1 &
  pid=$!
  sleep 8
  if kill -0 "$pid" 2>/dev/null; then
    if id=$("$tmp/helper" window "$pid") && screencapture -x -o -l "$id" "$out/$name.png"; then
      echo "saved $out/$name.png ($(sips -g pixelWidth -g pixelHeight "$out/$name.png" | awk '/pixel/ {printf "%s ", $2}'))"
    else
      echo "scene $name: no window to capture"
      failed="$failed $name"
    fi
    kill "$pid"; wait "$pid" 2>/dev/null
  else
    echo "scene $name: the app exited early"
    tail -n 20 "$out/$name.log"
    failed="$failed $name"
  fi
  if grep -q 'panicked' "$out/$name.log"; then
    echo "scene $name: a panic in the log"
    grep -A3 'panicked' "$out/$name.log"
    failed="$failed $name"
  fi
done

if [ -n "$failed" ]; then
  echo "failed:$failed"
  exit 1
fi
