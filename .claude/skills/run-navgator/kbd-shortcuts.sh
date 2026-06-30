#!/usr/bin/env bash
# Page keyboard text-editing shortcut smoke (LYK-1309).
#
# Catches the bug where NavGator forwarded page key events with an empty modifier set, so swervo's
# textinput ShortcutMatcher never saw CONTROL and Ctrl+A/C/X/V typed the literal letter instead of
# select-all/cut/copy/paste. This is an INTERACTION smoke (not a static render gate): it drives a
# real text field through type -> Ctrl+A -> Ctrl+X -> Ctrl+V and asserts the field empties on cut
# and refills on paste, measured by dark-ink pixel count in the field (no OCR needed).
#
#   Ctrl+A selects all  ->  Ctrl+X cuts (field empties)  ->  Ctrl+V pastes (field refills)
#
# If modifiers regress to not being forwarded: Ctrl+A/X/V type "axv" and the field never empties,
# so INK_CUT stays high and the gate fails.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/kbd-shortcuts; mkdir -p "$OUT"
PORT="${KS_PORT:-8993}"
# Field crop in display coords (1280x800 capture): the textarea is page (20,40)+600x120; the page
# renders below ~78px of chrome, so display ~ (22,120) .. (620,236).
CROP="${KS_CROP:-590:108:24:122}"   # w:h:x:y
CLICK_X="${KS_CLICK_X:-320}"; CLICK_Y="${KS_CLICK_Y:-178}"

( cd "$HERE/kbd-shortcuts" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
"$DRV" start "http://localhost:$PORT/k.html" >/dev/null 2>&1; sleep 5

ink() {  # count dark pixels in the field crop of a screenshot
  python3 - "$1" "$CROP" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("L"); px = img.load(); W, H = img.size
w, h, x, y = (int(v) for v in sys.argv[2].split(":"))
n = sum(1 for j in range(y, min(y+h, H)) for i in range(x, min(x+w, W)) if px[i, j] < 110)
print(n)
PY
}

"$DRV" click "$CLICK_X" "$CLICK_Y" >/dev/null 2>&1; sleep 1     # focus the field
"$DRV" type 'cut and paste gjpqy' >/dev/null 2>&1; sleep 1
"$DRV" shot "$OUT/typed.png" >/dev/null 2>&1
"$DRV" key ctrl+a >/dev/null 2>&1; sleep 1                      # select all
"$DRV" key ctrl+x >/dev/null 2>&1; sleep 1                      # cut -> empties
"$DRV" shot "$OUT/cut.png" >/dev/null 2>&1
"$DRV" key ctrl+v >/dev/null 2>&1; sleep 1                      # paste -> refills
"$DRV" shot "$OUT/paste.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

T=$(ink "$OUT/typed.png"); C=$(ink "$OUT/cut.png"); P=$(ink "$OUT/paste.png")
echo "--- page keyboard shortcuts (Ctrl+A select-all, Ctrl+X cut, Ctrl+V paste) ---"
echo "  field ink:  typed=$T  after-cut=$C  after-paste=$P"
# Pass: typing produced ink; cut cleared it to near-zero; paste restored most of it.
fail=0
[ "$T" -gt 200 ] || { echo "  [FAIL] typing produced no field ink ($T) — focus/typing broken"; fail=1; }
[ "$C" -lt $(( T / 4 )) ] || { echo "  [FAIL] Ctrl+A/Ctrl+X did not clear the field (cut ink $C vs typed $T) — select-all/cut not firing"; fail=1; }
[ "$P" -gt $(( T / 2 )) ] || { echo "  [FAIL] Ctrl+V did not restore the field (paste ink $P vs typed $T) — paste not firing"; fail=1; }
[ "$fail" -eq 0 ] && echo "  [PASS] select-all + cut + paste all fire (modifiers forwarded to the page)"
exit $fail
