#!/usr/bin/env bash
# Per-control Chrome-baseline test for form controls. Catches text VERTICAL-POSITIONING bugs
# (clipping, top-aligned labels, wrong baseline) that compare.sh's single whole-page SSIM dilutes
# to nothing — that's how Google's search-box clipping + too-high "Google Search"/"I'm Feeling
# Lucky" labels slipped through (whole-page ~0.96 while the bad controls were ~0.5; bug LYK-1299).
#
# Gate metric: the sub-pixel vertical INK CENTROID of each control's text, as a fraction of the
# control height, compared swervo-vs-Chrome. |Δ| must be <= FB_POS_TOL percent. This is the right
# metric for a *positioning* bug (it ignores font-rasterization differences that SSIM conflates).
#
# Tolerance: FB_POS_TOL=0.1 (%) as requested. NOTE: 0.1% (~0.05px on a 46px control) is right at the
# cross-engine sub-pixel RASTERIZATION FLOOR — swervo and Chrome both use FreeType but differ in
# hinting/AA by ~0.1-0.15px, so even correctly-positioned text reads ~0.2-0.4%. The LYK-1299 button
# fix lands at ~0.05-0.17%; text input / textarea sit at ~0.2-0.4% (NOT bugs — rasterization). For a
# non-flaky gate use FB_POS_TOL=0.5 (cleanly separates correct ~0.4% from real bugs, which are >5%).
# SSIM vs Chrome is reported too (informational; ~0.97 with the font fix, never ~1.0 cross-engine).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/forms-baseline; mkdir -p "$OUT"
PORT="${FB_PORT:-8993}"; W=560; H=400; TOP=78
TOL="${FB_POS_TOL:-0.1}"
# name:x:y:w:h — must match forms-baseline/forms.html's absolute positions (shared by both engines)
RECTS="textinput:20:20:380:46 submit:20:90:220:46 buttoninput:20:160:240:46 buttonelem:20:230:230:46 textarea:20:300:380:46"

google-chrome --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
  --window-size="$W,$H" --virtual-time-budget=2000 \
  --screenshot="$OUT/chrome.png" "file://$HERE/forms-baseline/forms.html" 2>/dev/null

( cd "$HERE/forms-baseline" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 </dev/null & ); sleep 1
"$DRV" start "http://localhost:$PORT/forms.html" >/dev/null 2>&1; sleep 6
"$DRV" shot "$OUT/swervo_full.png" >/dev/null 2>&1
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null
ffmpeg -nostdin -y -i "$OUT/swervo_full.png" -vf "crop=$W:$H:0:$TOP" "$OUT/swervo.png" </dev/null >/dev/null 2>&1

# SSIM per control (informational)
declare -A SSIM
for r in $RECTS; do
  IFS=: read -r n x y w h <<<"$r"
  ffmpeg -nostdin -y -i "$OUT/chrome.png" -vf "crop=$w:$h:$x:$y" "$OUT/c_$n.png" </dev/null >/dev/null 2>&1
  ffmpeg -nostdin -y -i "$OUT/swervo.png" -vf "crop=$w:$h:$x:$y" "$OUT/s_$n.png" </dev/null >/dev/null 2>&1
  SSIM[$n]=$(ffmpeg -nostdin -i "$OUT/c_$n.png" -i "$OUT/s_$n.png" -lavfi ssim -f null - </dev/null 2>&1 | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2)
done

# Vertical-position gate (the real check)
echo "--- form-control vertical position vs Chrome (gate: |Δ| <= ${TOL}% of control height) ---"
python3 - "$OUT" "$TOL" "$RECTS" "${SSIM[textinput]:-} ${SSIM[submit]:-} ${SSIM[buttoninput]:-} ${SSIM[buttonelem]:-} ${SSIM[textarea]:-}" <<'PY'
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
out, tol = sys.argv[1], float(sys.argv[2])
rects = sys.argv[3].split()
ssims = sys.argv[4].split()
chrome = Image.open(f"{out}/chrome.png").convert("L")
swervo = Image.open(f"{out}/swervo.png").convert("L")
def vfrac(img, x, y, w, h):
    c = img.crop((x, y, x + w, y + h)); W, H = c.size; px = c.load(); num = den = 0.0
    for j in range(H):
        for i in range(W):
            ink = 255 - px[i, j]
            if ink > 40: num += ink * (j + 0.5); den += ink
    return (num / den) / H if den else 0.5
fail = 0
for idx, r in enumerate(rects):
    n, x, y, w, h = r.split(":"); x, y, w, h = int(x), int(y), int(w), int(h)
    cf = vfrac(chrome, x, y, w, h); sf = vfrac(swervo, x, y, w, h)
    d = abs(cf - sf) * 100
    ok = "PASS" if d <= tol else "FAIL"
    if ok == "FAIL": fail = 1
    ss = ssims[idx] if idx < len(ssims) else "?"
    print(f"  [{ok}] {n:12s} Δpos={d:.3f}%  (chrome {cf*100:.1f}% / swervo {sf*100:.1f}%, SSIM={ss})")
sys.exit(fail)
PY
