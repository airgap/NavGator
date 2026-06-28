#!/usr/bin/env bash
# Per-control Chrome-baseline test for form controls — catches text vertical-positioning /
# clipping / font-metric divergences that whole-page SSIM (compare.sh) dilutes into nothing.
#
# Why this exists: the Google search-box clipping + "Google Search"/"I'm Feeling Lucky" too-high
# labels slipped through because compare.sh reports ONE whole-page SSIM (a few-px offset inside a
# couple of controls barely moves it — measured 0.905 whole-page while individual controls were
# 0.42-0.77) and regression.sh only checked form ACCENT COLOR. This crops EACH control tightly and
# SSIMs it against Chrome, so a small text offset is a large fraction of the cropped area and fails.
#
# forms-baseline/forms.html positions each control absolutely (identical coords in both engines),
# so the crops align. Usage: forms-baseline.sh   (exit 0 = all controls within tolerance).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRV="$HERE/driver.sh"
OUT=/tmp/forms-baseline; mkdir -p "$OUT"
PORT="${FB_PORT:-8993}"; W=560; H=400; TOP=78
THRESH="${FB_SSIM_MIN:-0.95}"
# name:x:y:w:h — must match forms.html's absolute positions (+ chrome/swervo share them)
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

fail=0
echo "--- per-control SSIM vs Chrome (threshold $THRESH) ---"
for r in $RECTS; do
  IFS=: read -r n x y w h <<<"$r"
  ffmpeg -nostdin -y -i "$OUT/chrome.png" -vf "crop=$w:$h:$x:$y" "$OUT/c_$n.png" </dev/null >/dev/null 2>&1
  ffmpeg -nostdin -y -i "$OUT/swervo.png" -vf "crop=$w:$h:$x:$y" "$OUT/s_$n.png" </dev/null >/dev/null 2>&1
  s=$(ffmpeg -nostdin -i "$OUT/c_$n.png" -i "$OUT/s_$n.png" -lavfi ssim -f null - </dev/null 2>&1 | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2)
  ok=$(awk -v s="${s:-0}" -v t="$THRESH" 'BEGIN{print (s>=t)?"PASS":"FAIL"}')
  printf '  [%s] %-12s SSIM=%s\n' "$ok" "$n" "${s:-NA}"
  [ "$ok" = FAIL ] && fail=1
done
sp=$(ffmpeg -nostdin -i "$OUT/chrome.png" -i "$OUT/swervo.png" -lavfi ssim -f null - </dev/null 2>&1 | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2)
echo "  (whole-page SSIM=$sp — what compare.sh reports; the per-control checks above are the gate)"
exit $fail
