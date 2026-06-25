#!/usr/bin/env bash
# swervo-vs-Chrome rendering comparison, for baselining + bug-hunting NavGator's Servo fork.
# Renders the SAME url in google-chrome (headless baseline) and swervo (the navgator driver) at
# the same content viewport, then emits a side-by-side PNG + an SSIM score (1.0 = identical).
#
# Usage:  compare.sh <name> <url>
#   e.g.  compare.sh google https://www.google.com/
# Output: /tmp/navgator-compare/<name>_{chrome,swervo,sidebyside}.png   (chrome LEFT, swervo RIGHT)
#
# Requires: google-chrome, ffmpeg, and the run-navgator driver (driver.sh, same dir).
set -uo pipefail
name="${1:?usage: compare.sh <name> <url>}"
url="${2:?usage: compare.sh <name> <url>}"
W=1280; VH=722                 # navgator's content viewport (1280x800 window minus ~78px chrome)
TOP=78                         # chrome height to crop off swervo's screenshot
OUT=/tmp/navgator-compare; mkdir -p "$OUT"
DRV="$(cd "$(dirname "$0")" && pwd)/driver.sh"

echo "[1/3] chrome baseline…"
google-chrome --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
  --window-size="$W,$VH" --virtual-time-budget=5000 \
  --screenshot="$OUT/${name}_chrome.png" "$url" 2>/dev/null

echo "[2/3] swervo (navgator)…"
"$DRV" start "$url" >/dev/null 2>&1
sleep 6                        # Servo + software GL needs a beat to load + paint
"$DRV" shot "$OUT/${name}_swervo_full.png" >/dev/null
"$DRV" stop >/dev/null 2>&1
ffmpeg -y -i "$OUT/${name}_swervo_full.png" -vf "crop=$W:$VH:0:$TOP" \
  "$OUT/${name}_swervo.png" >/dev/null 2>&1

echo "[3/3] diff…"
ssim=$(ffmpeg -i "$OUT/${name}_swervo.png" -i "$OUT/${name}_chrome.png" -lavfi ssim -f null - 2>&1 \
  | grep -oE 'All:[0-9.]+' | tail -1)
ffmpeg -y -i "$OUT/${name}_chrome.png" -i "$OUT/${name}_swervo.png" \
  -filter_complex hstack "$OUT/${name}_sidebyside.png" >/dev/null 2>&1
echo "SSIM ${ssim:-n/a} (1.0=identical)  ->  $OUT/${name}_sidebyside.png  (chrome | swervo)"
