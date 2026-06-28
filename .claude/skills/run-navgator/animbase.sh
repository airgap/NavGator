#!/usr/bin/env bash
# Baseline a CSS animation against Chrome at phase samples, DETERMINISTICALLY (no wall-clock race).
#
# Each sample freezes the animation at t = <pct>% of its duration by loading the page with
# `?t=<ms>&d=<dur>`, where the page pauses the animation and applies `animation-delay: -t` — a
# paused animation with negative delay renders the static frame at that offset. Negative pct →
# positive delay → before-phase; pct>100 → after-phase (both exercise animation-fill-mode). Because
# every frame is static, Chrome (--screenshot) and swervo (driver shot) capture the exact frame with
# no timing sensitivity. We then SSIM each pair and build a stacked contact sheet.
#
# Usage:  animbase.sh [page] [duration_ms]
#   page          file under run-navgator/animbase/ (default: seek.html)
#   duration_ms   the animation's duration in the page (default: 1000)
# Output: /tmp/anim-base/contact.png  + per-sample chrome/swervo/pair PNGs + an SSIM table.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
PAGE="${1:-seek.html}"
D="${2:-1000}"
W=1280; H=722                  # content viewport (matches driver window 1280x800 minus 78px chrome)
TOP=78
PORT="${ANIM_PORT:-8995}"
OUT=/tmp/anim-base; mkdir -p "$OUT"
SAMPLES="-5 -1 0 1 5 25 50 75 95 99 100 101 105"
FONT="$(fc-match -f '%{file}' DejaVuSans 2>/dev/null || echo /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf)"

( cd "$HERE/animbase" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 < /dev/null & ); sleep 1
url() { echo "http://localhost:$PORT/$PAGE?t=$1&d=$D"; }
label() { printf 'p%+04d' "$1"; }   # p+000, p-005, p+105 …

# swervo: start once, re-navigate per sample.
"$DRV" start "$(url 0)" >/dev/null 2>&1; sleep 5

declare -A SS
pairs=()
for p in $SAMPLES; do
  t=$(awk -v p="$p" -v d="$D" 'BEGIN{printf "%g", p/100*d}')
  lab="$(label "$p")"
  # chrome baseline (static frozen page; small budget just to let layout settle)
  google-chrome --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
    --window-size="$W,$H" --virtual-time-budget=1500 \
    --screenshot="$OUT/${lab}_chrome.png" "$(url "$t")" >/dev/null 2>&1
  # swervo
  "$DRV" nav "$(url "$t")" >/dev/null 2>&1; sleep 4
  "$DRV" shot "$OUT/${lab}_swervo_full.png" >/dev/null 2>&1
  ffmpeg -y -i "$OUT/${lab}_swervo_full.png" -vf "crop=$W:$H:0:$TOP" "$OUT/${lab}_swervo.png" >/dev/null 2>&1
  # ssim + labelled side-by-side (chrome | swervo)
  s=$(ffmpeg -i "$OUT/${lab}_chrome.png" -i "$OUT/${lab}_swervo.png" -lavfi ssim -f null - 2>&1 \
      | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2)
  SS[$p]="${s:-NA}"
  txt="${p}% t=${t}ms  chrome|swervo  SSIM=${s:-NA}"
  ffmpeg -y -i "$OUT/${lab}_chrome.png" -i "$OUT/${lab}_swervo.png" -filter_complex \
    "hstack,scale=900:-1,drawbox=y=0:h=22:c=black@0.55:t=fill,drawtext=fontfile=$FONT:text='$txt':x=8:y=3:fontsize=15:fontcolor=white" \
    "$OUT/${lab}_pair.png" >/dev/null 2>&1 || \
    ffmpeg -y -i "$OUT/${lab}_chrome.png" -i "$OUT/${lab}_swervo.png" -filter_complex "hstack,scale=900:-1" "$OUT/${lab}_pair.png" >/dev/null 2>&1
  pairs+=("$OUT/${lab}_pair.png")
  printf '  %-5s%% t=%-6sms  SSIM=%s\n' "$p" "$t" "${s:-NA}"
done
"$DRV" stop >/dev/null 2>&1
hp=$(ps -eo pid,args | awk -v P="$PORT" '$0 ~ "http.server "P && !/awk/{print $1}'); [ -n "$hp" ] && kill -9 $hp 2>/dev/null

# stacked contact sheet (PIL — ffmpeg vstack is flaky with this many inputs)
python3 - "$OUT/contact.png" "${pairs[@]}" <<'PY' && echo "contact sheet: $OUT/contact.png"
import sys, warnings; warnings.filterwarnings("ignore")
from PIL import Image
out, paths = sys.argv[1], sys.argv[2:]
imgs = [Image.open(p).convert("RGB") for p in paths]
w = min(i.width for i in imgs)
imgs = [i.resize((w, round(i.height * w / i.width))) for i in imgs]
sheet = Image.new("RGB", (w, sum(i.height for i in imgs)), "white"); y = 0
for i in imgs: sheet.paste(i, (0, y)); y += i.height
sheet.save(out)
PY

echo "--- SSIM by phase (1.0 = identical to Chrome) ---"
for p in $SAMPLES; do awk -v p="$p" -v s="${SS[$p]}" 'BEGIN{printf "  %5s%%  %s  %s\n", p, s, (s+0>=0.95?"":"<-- diverges")}'; done
