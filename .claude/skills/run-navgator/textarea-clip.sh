#!/usr/bin/env bash
# <textarea> descender-clip regression gate (LYK-1301).
#
# Catches the bug where a flex-stretched single-line <textarea> (Google's gLFyf search box sets
# `display:flex` on the <textarea> host) collapsed its content box below one line, so the text line
# overflowed the too-short scrollport and `overflow:hidden` sheared the descenders (g/j/p/q/y). The
# fix is a UA `textarea { min-block-size: 1lh }` floor (swervo). forms-baseline.sh never caught it:
# it renders a plain block <textarea>, never a flex-stretched one; the whole-page SSIM in compare.sh
# dilutes a few sheared pixels to nothing.
#
# Gate: render the exact gLFyf trigger (TEST) next to a tall block <textarea> that cannot clip (REF),
# same font/size/descender text. Measure each text's ink-bbox HEIGHT. With the fix, TEST keeps its
# descenders so TEST_h ~= REF_h; if the clip regresses, TEST loses its descenders and TEST_h shrinks.
# Assert TEST_h / REF_h >= THRESH. A regression reads as ~0.75 (descenders are ~1/4 of the ink height).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/textarea-clip; mkdir -p "$OUT"
PORT="${TC_PORT:-8994}"; TOP=93
THRESH="${TC_THRESH:-0.90}"
# name:x:y:w:h — page-content crop rects (add TOP for the swervo capture's chrome offset).
# TEST line sits at content y = 20(box) + 14(pad) .. ; REF at y = 120 .. — crop generously.
RECTS="TEST:18:48:300:72 REF:18:196:300:72"

( cd "$HERE/textarea-clip" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
"$DRV" start "http://localhost:$PORT/clip.html" >/dev/null 2>&1; sleep 6
"$DRV" shot "$OUT/swervo_full.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "--- <textarea> descender clip (gate: flex-textarea keeps descenders, TEST/REF ink height >= ${THRESH}) ---"
python3 - "$OUT/swervo_full.png" "$THRESH" "$TOP" "$RECTS" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("L")
thresh = float(sys.argv[2]); top = int(sys.argv[3]); rects = sys.argv[4].split()
px = img.load(); W, H = img.size
def ink_h(x, y, w, h):
    ys = []
    for j in range(y, min(y + h, H)):
        for i in range(x, min(x + w, W)):
            if px[i, j] < 120:
                ys.append(j); break
    return (max(ys) - min(ys) + 1) if ys else 0
vals = {}
for r in rects:
    n, x, y, w, h = r.split(":"); x, y, w, h = int(x), int(y) + top, int(w), int(h)
    vals[n] = ink_h(x, y, w, h)
test, ref = vals.get("TEST", 0), vals.get("REF", 0)
ratio = test / ref if ref else 0.0
ok = ratio >= thresh
print(f"  TEST ink height={test}px  REF ink height={ref}px  ratio={ratio:.2f}")
print(f"  [{'PASS' if ok else 'FAIL'}] flex-stretched <textarea> descenders {'intact' if ok else 'SHEARED'} (>= {thresh})")
sys.exit(0 if ok else 1)
PY
