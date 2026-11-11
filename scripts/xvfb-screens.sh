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

# A run whose app quit before its screenshot crashed: noted here, and the script fails
# at the end (after handing back the pictures).
failed=""
shot() {
  kill -0 "$pid" 2>/dev/null || { echo "$1: the app exited early" >&2; failed="$failed $1"; }
  import -window root "$out/$1.png" 2>/dev/null
}
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
  echo "$1: $(grep -E 'VmRSS|VmHWM' /proc/$pid/status | paste -sd ' ')" >> "$out/memory.txt"
  kill $pid; wait $pid 2>/dev/null
}
scene 06-chat-cozy chat-cozy c-general
scene 07-chat-compact chat-compact c-general
scene 08-chat-system chat-system c-system
scene 09-chat-replies chat-replies c-replies
scene 10-markdown-inline markdown-inline c-dev
scene 11-markdown-blocks markdown-blocks c-dev
scene 12-markdown-alerts markdown-alerts c-dev
scene 13-markdown-code markdown-code c-dev
scene 16-attachments attachments c-attachments
scene 17-embed-all-types embed-all-types c-embeds
scene 18-image-viewer image-viewer c-attachments
scene 19-chat-reactions chat-reactions c-general
scene 20-chat-menu chat-menu c-general
scene 21-chat-delete chat-delete c-general

# 6. The Desktop settings tab, and the prompt a link from outside opens.
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL=c-general INLET_DEMO_SETTINGS=desktop "$bin" >>"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 14-settings-desktop
kill $pid; wait $pid 2>/dev/null
INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL=c-general INLET_DEMO_DEEP_LINK=inlet://invite/abc123 "$bin" >>"$out/demo.log" 2>&1 &
pid=$!
sleep 6
shot 15-link-prompt
kill $pid; wait $pid 2>/dev/null

# 7. Several accounts: the account switcher (two more accounts in the background, one
#    on another instance) and the sign-in screen when adding one.
scene 22-accounts accounts c-general
scene 23-accounts-add accounts-add c-general

# 8. The composer with files attached and an upload on its way.
scene 24-composer-attachments composer-attachments c-general

tail -n 20 "$out/signin.log" "$out/demo.log" >&2
cat "$out/memory.txt" >&2
rm -rf "$out/config"
tar -C "$out" -cf - .
if [ -n "$failed" ]; then
  echo "exited early:$failed" >&2
  exit 1
fi
