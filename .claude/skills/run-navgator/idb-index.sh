#!/usr/bin/env bash
# IndexedDB index-support gate (LYK-1310).
#
# Exercises the full IDBIndex read surface that swervo gained in LYK-1310 — get / getKey / getAll /
# getAllKeys / count / openCursor (with continue() iterating every record) — over a store with a
# non-unique and a unique index. The fixture self-validates each result and paints the page
# background GREEN on all-pass / RED on any failure, so this gate just samples a background pixel
# (OCR-free). Before this feature every index query threw "not a function" (broke YouTube's
# IndexedDB), and no cursor could advance past its first record.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/idb-index; mkdir -p "$OUT"
PORT="${IDB_PORT:-8988}"

( cd "$HERE/idb-index" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" stop >/dev/null 2>&1
"$DRV" start "http://localhost:$PORT/idb.html" >/dev/null 2>&1; sleep 6
"$DRV" shot "$OUT/result.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "--- IndexedDB index support (get/getKey/getAll/getAllKeys/count/openCursor+continue) ---"
python3 - "$OUT/result.png" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB"); W, H = img.size
# Sample a patch low on the page where the body background shows (below the log text).
xs = range(W // 4, 3 * W // 4, 7); ys = range(int(H * 0.6), int(H * 0.9), 7)
pts = [img.getpixel((x, y)) for x in xs for y in ys]
green = sum(1 for r, g, b in pts if g > 110 and r < 90 and b < 90)
red   = sum(1 for r, g, b in pts if r > 110 and g < 90 and b < 90)
total = len(pts)
print(f"  background sample: green={green}/{total} red={red}/{total}")
if green > total * 0.5:
    print("  [PASS] all index operations succeeded (green)")
    sys.exit(0)
elif red > total * 0.5:
    print("  [FAIL] an index operation failed (red) — see the page log")
    sys.exit(1)
else:
    print("  [FAIL] no pass/fail signal (page did not finish — likely a hang/crash)")
    sys.exit(1)
PY
