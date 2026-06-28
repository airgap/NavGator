#!/usr/bin/env bash
# Inline-<svg> aspect-ratio regression gate (LYK-1300).
#
# Catches the class of bug that vertically stretched Google's search-box clear "X": swervo
# rasterized an inline <svg> by NON-uniformly scaling its viewBox to fill the box, ignoring the
# SVG viewport's `preserveAspectRatio` (default `xMidYMid meet`). When layout gave the <svg> box a
# different aspect ratio than its (square) viewBox — a flex/grid item, `height:100%`, an auto cross
# size — the glyph distorted. forms-baseline.sh never caught it: it only renders <input>/<button>/
# <textarea>, never an inline <svg> icon.
#
# Gate: every icon in svg-aspect/icons.html has a SQUARE viewBox (0 0 24 24) but a NON-square box.
# Per `meet`, each rendered glyph's ink bounding box must stay ~square. We assert |h/w - 1| <= TOL.
# A distortion regression reads as h/w ~= 2+ (the original clear-X was ~1.8). TOL default 0.18
# absorbs sub-pixel AA + the glyphs' own slight non-squareness, while failing real distortion.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/svg-aspect; mkdir -p "$OUT"
PORT="${SA_PORT:-8995}"; W=1280; H=800; TOP=93
TOL="${SA_ASPECT_TOL:-0.18}"
# name:x:y:w:h — page-content rects (add TOP for the swervo capture's chrome offset). The first
# three boxes are 24x60 (tall, cross stretched); D is the 60x60 square control.
RECTS="A:20:20:24:60 B:80:20:24:60 C:140:20:24:60 D:220:20:60:60"

( cd "$HERE/svg-aspect" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
"$DRV" start "http://localhost:$PORT/icons.html" >/dev/null 2>&1; sleep 6
"$DRV" shot "$OUT/swervo_full.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "--- inline-<svg> aspect ratio (gate: each square-viewBox glyph stays ~square, |h/w-1| <= ${TOL}) ---"
python3 - "$OUT/swervo_full.png" "$TOL" "$TOP" "$RECTS" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("L")
tol = float(sys.argv[2]); top = int(sys.argv[3]); rects = sys.argv[4].split()
px = img.load(); W, H = img.size
def ink_bbox(x, y, w, h):
    xs = []; ys = []
    for j in range(y, min(y + h, H)):
        for i in range(x, min(x + w, W)):
            if px[i, j] < 120:
                xs.append(i); ys.append(j)
    if not xs: return None
    return (max(xs) - min(xs) + 1, max(ys) - min(ys) + 1)
fail = 0
for r in rects:
    n, x, y, w, h = r.split(":"); x, y, w, h = int(x), int(y) + top, int(w), int(h)
    bb = ink_bbox(x, y, w, h)
    if not bb:
        print(f"  [FAIL] {n}: no glyph ink found"); fail = 1; continue
    iw, ih = bb
    ratio = ih / iw if iw else 99.0
    ok = abs(ratio - 1.0) <= tol
    if not ok: fail = 1
    print(f"  [{'PASS' if ok else 'FAIL'}] {n}: ink {iw}x{ih}  h/w={ratio:.2f}")
sys.exit(fail)
PY