#!/usr/bin/env bash
# Window-resize reflow regression gate.
#
# Catches the class of bug where a window resize resizes the framebuffer but the page LAYOUT
# never hears about the new viewport — pages stay laid out at the old size forever (stale
# content top-left + white L after growing; cropped layout after shrinking; even reloads lay
# out stale). Root cause the first time: render_pane resized the pane's OffscreenRenderingContext
# directly BEFORE WebView::resize, so the engine painter's resize path early-returned on
# "context already at target size" and never pushed the new rect to layout. WebView::resize
# must be the only resize entry point for a pane with tabs.
#
# Nothing else in the suite resizes the window (and desktop use is mostly maximized), which is
# how that bug shipped unseen for weeks — hence this gate.
#
# Gate: reflow.html pins a 24px red marker fixed to the viewport's bottom-right corner. After
# shrinking the window, the marker must sit at the NEW content-area corner. Stale layout leaves
# it at the old (now cropped) corner, so no red ink lands in the probe box -> FAIL. A green
# fixed full-viewport layer doubles as a page-actually-loaded sanity check.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/resize-reflow; mkdir -p "$OUT"
PORT="${RR_PORT:-8996}"
# Window is 1280x800 on start; shrink to:
SW=1000; SH=600

( cd "$HERE/resize-reflow" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
# The driver's start reuses ANY Xvfb already on :99 (regression.sh leaves a 900x620 one behind),
# but this gate needs the full 1280x800 screen to resize within. Recycle :99 if it's the wrong size.
DISP="${NAVG_DISPLAY:-:99}"
geom="$(DISPLAY="$DISP" xdpyinfo 2>/dev/null | awk '/dimensions:/{print $2}')"
if [ "$geom" != "1280x800" ]; then
  xp=$(pgrep -f "Xvfb $DISP " | head -1); [ -n "$xp" ] && kill "$xp" 2>/dev/null && sleep 1
  rm -f "/tmp/.X${DISP#:}-lock"
fi
"$DRV" start "http://localhost:$PORT/reflow.html" >/dev/null 2>&1; sleep 5
WID="$(DISPLAY="$DISP" xdotool search --name NavGator | head -1)"
DISPLAY="$DISP" xdotool windowsize "$WID" "$SW" "$SH"; sleep 4
"$DRV" shot "$OUT/after_shrink.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "--- resize reflow (gate: fixed bottom-right marker tracks the ${SW}x${SH} window corner) ---"
python3 - "$OUT/after_shrink.png" "$SW" "$SH" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB")
sw, sh = int(sys.argv[2]), int(sys.argv[3])
px = img.load()
def count(x0, y0, x1, y1, pred):
    return sum(1 for y in range(y0, y1) for x in range(x0, x1) if pred(px[x, y]))
# Sanity: the green viewport layer painted (page loaded) - sample mid-content.
green = count(sw//2 - 25, sh//2, sw//2 + 25, sh//2 + 50,
              lambda c: c[0] < 90 and c[1] > 140 and c[2] < 90)
# Gate: red marker ink inside the 50x50 box at the shrunken window's bottom-right corner.
red = count(sw - 50, sh - 50, sw, sh,
            lambda c: c[0] > 160 and c[1] < 80 and c[2] < 80)
ok_green = green > 1000
ok_red = red > 100
print(f"  [{'PASS' if ok_green else 'FAIL'}] page loaded (green viewport layer px={green}, > 1000)")
print(f"  [{'PASS' if ok_red else 'FAIL'}] layout reflowed (corner marker px={red}, > 100)")
sys.exit(0 if (ok_green and ok_red) else 1)
PY
