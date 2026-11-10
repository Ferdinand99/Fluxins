#!/usr/bin/env bash
# Runs the app under Xvfb (software Vulkan) and takes screenshots of the sign-in screen
# and of the demo data. For headless checks on a Linux build machine.
# Usage: scripts/xvfb-screens.sh <path to inlet binary>   (writes a tar of PNGs to stdout)
set -uo pipefail
bin="$1"
out=$(mktemp -d)
Xvfb :99 -screen 0 1280x860x24 >/dev/null 2>&1 &
export DISPLAY=:99 XDG_CONFIG_HOME="$out/config"
sleep 2

shot() { import -window root "$out/$1.png" 2>/dev/null; }
win() { xdotool search --name '^Inlet' | head -1; }
click() { xdotool mousemove --window "$(win)" "$1" "$2" click 1; sleep 1.5; }

# 1. Sign-in screen (no saved session)
"$bin" >"$out/signin.log" 2>&1 &
pid=$!
sleep 6
shot 01-sign-in
# switch to the token tab
kill $pid; wait $pid 2>/dev/null

# 2. Demo data
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=4 "$bin" >"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 02-direct-messages
click 36 102         # first guild in the rail
shot 03-guild-welcome
click 192 172        # "# general"
shot 04-general
xdotool type --delay 30 'Hello from **Inlet** on Xvfb'
xdotool key Return
sleep 5              # a live demo message arrives too
shot 05-sent-and-live
grep -E 'VmRSS|VmHWM' /proc/$pid/status > "$out/memory.txt"
kill $pid; wait $pid 2>/dev/null

tail -20 "$out/signin.log" "$out/demo.log" >&2
cat "$out/memory.txt" >&2
rm -rf "$out/config"
tar -C "$out" -cf - .
