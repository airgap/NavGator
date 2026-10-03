#!/usr/bin/env bash
# document.all gate (HTMLAllCollection + [[IsHTMLDDA]]).
#
# The fixture checks document.all's falsy-but-not-undefined semantics (incl. after JIT warm-up),
# the collection's indexed/named/callable lookups, and polymer-resin's exact
# `c || c === document.all` test, then paints the page GREEN on all-pass / RED on any failure;
# this gate samples a background pixel (OCR-free). Before HTMLAllCollection existed, document.all
# was plain `undefined`, so polymer-resin "sanitized" every undefined binding into a truthy
# placeholder: YouTube hid its search results and dropped its guide icons.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/document-all; mkdir -p "$OUT"
PORT="${DOCALL_PORT:-8989}"

( cd "$HERE/document-all" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
"$DRV" start "http://localhost:$PORT/all.html" >/dev/null 2>&1; sleep 6
"$DRV" shot "$OUT/result.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "--- document.all (HTMLAllCollection, [[IsHTMLDDA]], [[Call]]) ---"
python3 - "$OUT/result.png" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB"); W, H = img.size
# The log occupies the left of the page; sample the background to its right.
xs = range(int(W * 0.7), int(W * 0.95), 7); ys = range(int(H * 0.2), int(H * 0.9), 7)
pts = [img.getpixel((x, y)) for x in xs for y in ys]
green = sum(1 for r, g, b in pts if g > 110 and r < 90 and b < 90)
red   = sum(1 for r, g, b in pts if r > 110 and g < 90 and b < 90)
total = len(pts)
print(f"  background sample: green={green}/{total} red={red}/{total}")
if green > total * 0.5:
    print("  [PASS] all document.all checks passed (green)")
    sys.exit(0)
elif red > total * 0.5:
    print(f"  [FAIL] a document.all check failed (red) — Read {sys.argv[1]} for the per-check log")
    sys.exit(1)
else:
    print("  [FAIL] no pass/fail signal (page did not finish — likely a script error or crash)")
    sys.exit(1)
PY
