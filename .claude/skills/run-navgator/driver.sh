#!/usr/bin/env bash
# NavGator headless run/drive harness — see SKILL.md.
#
# NavGator is a native winit + egui + Servo GUI (NOT Electron / not a CDP-driveable browser:
# the chrome is egui, the page is Servo). So we drive it the only way a native X11 app can be
# driven headlessly: a virtual X display (Xvfb + Mesa llvmpipe software GL), xdotool for input,
# ffmpeg x11grab for screenshots. Each subcommand is a thing that was run by hand on a live
# /run and worked.
#
# Usage:
#   driver.sh build                 # cargo build -p navgator (run via Bash dangerouslyDisableSandbox)
#   driver.sh start [url]           # start Xvfb :99 + launch navgator (default gator://welcome)
#   driver.sh shot [file]           # screenshot the display -> PNG (default $STATE/shot.png)
#   driver.sh nav <url>             # focus address bar, type url, Enter
#   driver.sh palette               # open the command palette (Ctrl+K)
#   driver.sh key  <xdotool-keys>   # e.g. driver.sh key ctrl+t   /   driver.sh key Return
#   driver.sh type <text...>        # type into whatever is focused
#   driver.sh click <x> <y>         # move mouse + left click (display coords, 1280x800)
#   driver.sh stop                  # kill ONLY what this driver started (never touches :0)
#
# IMPORTANT: this driver only ever touches its own Xvfb :99 and the navgator PID it launched.
# The user runs their own NavGator AppImages on the real display :0 — never kill by name.
set -uo pipefail

DISP="${NAVG_DISPLAY:-:99}"
W=1280; H=800
BIN="./target/debug/navgator"
STATE="/tmp/navgator-run"
mkdir -p "$STATE"

find_wid() { DISPLAY="$DISP" xdotool search --name NavGator 2>/dev/null | head -1; }

# No window manager runs under Xvfb, so a click sets pointer focus but NOT X input focus.
# We must explicitly windowfocus the navgator window before sending XTEST keystrokes.
focus() { local w; w="$(find_wid)"; [ -n "$w" ] && DISPLAY="$DISP" xdotool windowfocus "$w" && sleep 0.2; }

cmd="${1:-}"; shift 2>/dev/null || true
case "$cmd" in
  build)
    # Needs the Bash tool's dangerouslyDisableSandbox (libclang/codegen). Incremental ~3-10s.
    cargo build -p navgator
    ;;

  start)
    url="${1:-gator://welcome}"
    # ISOLATE the profile: navgator honors XDG_CONFIG_HOME (main.rs:590,782). Point it at a throwaway
    # dir so test runs never pollute the user's real ~/.config/navgator (history, permissions, session,
    # passwords). Persisted across runs under $STATE so the app's own state is stable; wipe with `stop`.
    export XDG_CONFIG_HOME="$STATE/profile"
    mkdir -p "$XDG_CONFIG_HOME"
    if ! pgrep -f "Xvfb $DISP " >/dev/null 2>&1; then
      setsid Xvfb "$DISP" -screen 0 "${W}x${H}x24" +extension GLX +render -noreset \
        >"$STATE/xvfb.log" 2>&1 < /dev/null &
      echo $! > "$STATE/xvfb.pid"
      sleep 2
    fi
    # setsid so navgator survives this script exiting; takes a URL as argv[1].
    setsid env DISPLAY="$DISP" "$BIN" "$url" >"$STATE/nav.log" 2>&1 < /dev/null &
    echo $! > "$STATE/nav.pid"
    # Servo + software GL takes a few seconds; wait for the window to map.
    for _ in $(seq 1 40); do [ -n "$(find_wid)" ] && break; sleep 0.5; done
    wid="$(find_wid)"
    [ -n "$wid" ] && echo "started: wid=$wid pid=$(cat "$STATE/nav.pid")" || { echo "FAILED to map window; see $STATE/nav.log"; tail -5 "$STATE/nav.log"; exit 1; }
    ;;

  shot)
    out="${1:-$STATE/shot.png}"
    # -draw_mouse 0: do NOT capture the X cursor. Xvfb's default cursor is an X shape that sits at
    # screen-center on launch — without this it shows up as a bogus "✕ glitch" mid-page (see #41).
    ffmpeg -y -draw_mouse 0 -f x11grab -video_size "${W}x${H}" -i "${DISP}.0" -frames:v 1 "$out" >/dev/null 2>&1 \
      && echo "$out" || { echo "screenshot failed"; exit 1; }
    ;;

  nav)
    [ -z "${1:-}" ] && { echo "nav needs a url"; exit 1; }
    focus
    DISPLAY="$DISP" xdotool mousemove 400 21 click 1; sleep 0.4   # the address-bar pill
    DISPLAY="$DISP" xdotool key ctrl+a; sleep 0.2
    DISPLAY="$DISP" xdotool type --delay 20 "$1"; sleep 0.2
    DISPLAY="$DISP" xdotool key Return
    ;;

  palette) focus; DISPLAY="$DISP" xdotool key ctrl+k ;;   # Ctrl+K = the ⌘K command palette
  key)     focus; DISPLAY="$DISP" xdotool key "$@" ;;
  type)    focus; DISPLAY="$DISP" xdotool type --delay 25 "$*" ;;
  click)   focus; DISPLAY="$DISP" xdotool mousemove "$1" "$2" click 1 ;;

  stop)
    # Kill ONLY our recorded PIDs (+ their children). Never pkill -f 'navgator' — that matches
    # the user's AppImages on :0 AND this very shell (whose args contain the string -> exit 144).
    if [ -f "$STATE/nav.pid" ]; then p="$(cat "$STATE/nav.pid")"; pkill -9 -P "$p" 2>/dev/null; kill -9 "$p" 2>/dev/null; fi
    [ -f "$STATE/xvfb.pid" ] && kill -9 "$(cat "$STATE/xvfb.pid")" 2>/dev/null
    rm -f "$STATE/nav.pid" "$STATE/xvfb.pid"
    echo "stopped"
    ;;

  *)
    echo "usage: driver.sh {build|start [url]|shot [file]|nav <url>|palette|key <k>|type <text>|click <x> <y>|stop}"
    exit 1
    ;;
esac
