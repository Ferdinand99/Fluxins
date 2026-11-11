#!/usr/bin/env bash
# Runs the app under Xvfb (software Vulkan) and takes screenshots of the sign-in screen
# and of the demo data, and of the inside of the window for every scene file in
# tests/visual/scenes (compared with the baselines by inlet-visual; they land in
# visual/). For headless checks on a Linux build machine and in CI.
# Usage: scripts/xvfb-screens.sh <path to inlet binary>   (writes a tar of PNGs to stdout)
# Navigation uses the demo variables and scene files (demo builds only), not clicks, so
# layout changes do not break the script.
set -uo pipefail
bin="$1"
scenes_dir="$(cd "$(dirname "$0")/.." && pwd)/tests/visual/scenes"
out=$(mktemp -d)
Xvfb :99 -screen 0 1280x860x24 -dpi 96 >/dev/null 2>&1 &
export DISPLAY=:99 XDG_CONFIG_HOME="$out/config"
# One device pixel per logical pixel, whatever the X server says.
export GPUI_X11_SCALE_FACTOR=1
sleep 2

# A run whose app quit before its screenshot crashed: noted here, and the script fails
# at the end (after handing back the pictures).
failed=""
# The app's window on the numbered runs' screen.
win() { xdotool search --name '^Inlet' | head -1; }
# A panic or a RefCell borrow error (a frame gpui dropped) in log $1 fails run $2.
bad_log() {
  if grep -qE 'panicked|already (mutably )?borrowed' "$1"; then
    echo "$2: a panic or borrow error in the log" >&2
    grep -E -A3 'panicked|already (mutably )?borrowed' "$1" >&2
    failed="$failed $2"
  fi
}
shot() {
  kill -0 "$pid" 2>/dev/null || { echo "$1: the app exited early" >&2; failed="$failed $1"; }
  import -window root "$out/$1.png" 2>/dev/null
}

# 1. Sign-in screen (no saved session)
"$bin" >"$out/signin.log" 2>&1 &
pid=$!
sleep 6
shot 01-sign-in
# switch to the token tab
kill $pid; wait $pid 2>/dev/null

# 2. Demo data, opened with INLET_DEMO_GUILD/INLET_DEMO_CHANNEL.
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
# The panels under the channel header: pinned messages, and the inbox's three tabs.
scene 24-chat-pins chat-pins c-general
scene 25-inbox-unreads inbox-unreads c-general
scene 26-inbox-saved inbox-saved c-general
scene 27-inbox-mentions inbox-mentions c-general

# 8. Custom emoji and stickers; 100 animated custom emoji. Each with the CPU the app
#    takes over 10 s (a share of one core) and how often it redrew.
cpu_scene() {
  RUST_LOG=info INLET_DEMO=1 INLET_DEMO_LIVE_SECS=3600 INLET_DEMO_SCENE="$2" INLET_DEMO_GUILD=g1 INLET_DEMO_CHANNEL=c-dev "$bin" >"$out/$1.log" 2>&1 &
  pid=$!
  sleep 6
  # Without a window manager nothing has the focus (and animations stop): give it.
  xdotool windowfocus "$(win)" 2>/dev/null; sleep 1
  c0=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || echo 0)
  sleep 10
  c1=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || echo 0)
  echo "$1: cpu $(( (c1 - c0) * 100 / $(getconf CLK_TCK) / 10 )) % of one core over 10 s" >> "$out/memory.txt"
  shot "$1"
  echo "$1: $(grep -E 'VmRSS|VmHWM' /proc/$pid/status | paste -sd ' ')" >> "$out/memory.txt"
  kill $pid; wait $pid 2>/dev/null
}
cpu_scene 28-custom-emoji custom-emoji
cpu_scene 29-emoji-100 emoji-100

# 9. The scene files of the visual checks. Xvfb's pointer cannot be moved and sits in
#    the middle of the screen; the demo puts a scene's window in the top left corner,
#    and the screen is more than twice its size, so the pointer is never over it (no
#    hover states). The app reads each file itself.
mkdir -p "$out/visual"
Xvfb :98 -screen 0 2600x1800x24 -dpi 96 >/dev/null 2>&1 &
xvfb_scenes=$!
sleep 2
# Where the pointer is (it cannot be moved); every scene window must stay clear of it.
pointer=$(DISPLAY=:98 xdotool getmouselocation --shell)
px=$(printf '%s\n' "$pointer" | sed -n 's/^X=//p')
py=$(printf '%s\n' "$pointer" | sed -n 's/^Y=//p')
echo "scene pointer: $px,$py" >&2
for file in "$scenes_dir"/*.toml; do
  name=$(sed -n 's/^name *= *"\([^"]*\)".*/\1/p' "$file" | head -1)
  size=$(sed -n 's/^window *= *\[ *\([0-9]*\) *, *\([0-9]*\) *\].*/\1 \2/p' "$file" | head -1)
  read -r sw sh <<<"$size"
  if [ -z "$sw" ] || [ -z "$sh" ] || [ "$sw" -ge "$px" ] || [ "$sh" -ge "$py" ]; then
    echo "scene $name: window ${sw}x${sh} reaches the pointer at $px,$py; make the screen larger" >&2
    failed="$failed scene-$name"
    continue
  fi
  DISPLAY=:98 RUST_LOG=info INLET_DEMO_SCENE="$file" "$bin" >"$out/visual/$name.log" 2>&1 &
  pid=$!
  sleep 8
  if kill -0 "$pid" 2>/dev/null; then
    id=$(DISPLAY=:98 xdotool search --onlyvisible --pid "$pid" 2>/dev/null | head -1)
    [ -n "$id" ] || id=$(DISPLAY=:98 xdotool search --onlyvisible --name "^Inlet" | head -1)
    # The window as placed: the pointer must be outside it, or hover states show.
    eval "$(DISPLAY=:98 xdotool getwindowgeometry --shell "$id" 2>/dev/null | grep -E '^(X|Y|WIDTH|HEIGHT)=[0-9]+$')"
    if [ -z "${WIDTH:-}" ] || { [ "$px" -ge "$X" ] && [ "$px" -lt $((X + WIDTH)) ] && [ "$py" -ge "$Y" ] && [ "$py" -lt $((Y + HEIGHT)) ]; }; then
      echo "scene $name: the pointer at $px,$py is over the window (${X:-?},${Y:-?} ${WIDTH:-?}x${HEIGHT:-?}) or the window was not found" >&2
      failed="$failed scene-$name"
    fi
    unset X Y WIDTH HEIGHT
    if ! DISPLAY=:98 import -window "$id" "$out/visual/$name.png" 2>/dev/null; then
      echo "scene $name: no window" >&2; failed="$failed scene-$name"
    fi
    kill "$pid"; wait "$pid" 2>/dev/null
  else
    echo "scene $name: the app exited early" >&2
    tail -n 20 "$out/visual/$name.log" >&2
    failed="$failed scene-$name"
  fi
  bad_log "$out/visual/$name.log" "scene-$name"
done
kill "$xvfb_scenes" 2>/dev/null

# The numbered runs' logs: a panic on another thread, or a borrow error gpui logs when
# it drops a frame, can leave the window up.
bad_log "$out/signin.log" signin
bad_log "$out/demo.log" demo

tail -n 20 "$out/signin.log" "$out/demo.log" >&2
grep -H "demo animations" "$out/28-custom-emoji.log" "$out/29-emoji-100.log" >&2
cat "$out/memory.txt" >&2
rm -rf "$out/config"
tar -C "$out" -cf - .
if [ -n "$failed" ]; then
  echo "failed:$failed" >&2
  exit 1
fi
