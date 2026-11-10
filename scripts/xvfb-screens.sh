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
# Press and release with a pause: gpui's inline text (links, spoilers) registers its
# mouse-up handler on the frame after the mouse-down, so an instant click is lost.
click() {
  xdotool mousemove --window "$(win)" "$1" "$2"; sleep 0.3
  xdotool mousedown 1; sleep 0.3; xdotool mouseup 1; sleep 1.5
}

# 1. Sign-in screen (no saved session)
"$bin" >"$out/signin.log" 2>&1 &
pid=$!
sleep 6
shot 01-sign-in
# switch to the token tab
kill $pid; wait $pid 2>/dev/null

# 2. Demo data. Navigation uses INLET_DEMO_GUILD/INLET_DEMO_CHANNEL (demo builds only)
#    instead of clicks, so layout changes do not break the script.
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=4 INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL=c-general INLET_DEMO_ACCENT="#ff7e00" "$bin" >"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 02-general
xdotool type --delay 30 'Hello from **Inlet** on Xvfb'
xdotool key Return
sleep 5              # a live demo message arrives too
shot 03-sent-and-live
grep -E 'VmRSS|VmHWM' /proc/$pid/status > "$out/memory.txt"
kill $pid; wait $pid 2>/dev/null

# 3. Embed-only bot messages and a message without anything.
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL=c-joins "$bin" >>"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 04-embeds
kill $pid; wait $pid 2>/dev/null

# 4. Direct messages (the start view).
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 "$bin" >>"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 05-direct-messages
kill $pid; wait $pid 2>/dev/null

# 5. The message list scenes.
scene() {
  INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 INLET_DEMO_SCENE="$2" INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL="$3" "$bin" >>"$out/demo.log" 2>&1 &
  pid=$!
  sleep 6
  shot "$1"
  kill $pid; wait $pid 2>/dev/null
}
scene 06-chat-cozy chat-cozy c-general
scene 07-chat-compact chat-compact c-general
scene 08-chat-system chat-system c-system
scene 09-chat-replies chat-replies c-replies
scene 10-markdown-inline markdown-inline c-dev
scene 11-markdown-blocks markdown-blocks c-dev
scene 12-markdown-code markdown-code c-dev

tail -n 20 "$out/signin.log" "$out/demo.log" >&2
cat "$out/memory.txt" >&2
rm -rf "$out/config"
tar -C "$out" -cf - .
