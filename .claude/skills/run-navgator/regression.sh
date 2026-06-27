#!/usr/bin/env bash
# NavGator/swervo rendering REGRESSION suite. Self-reftests: render a `test` page and a `ref`
# page in swervo and assert they look identical (SSIM) — so a future engine rev that breaks a
# rendering feature makes the test diverge from its reference. No Chrome and no golden images.
# Plus color assertions for cases with no shape-equivalent (e.g. form-control accent color).
#
# Run this AFTER bumping the swervo rev in crates/navgator-engine/Cargo.toml (and rebuilding):
#     cargo build -p navgator && .claude/skills/run-navgator/regression.sh
# Exit code 0 = all pass, non-zero = at least one regression. See run-navgator/SKILL.md.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DIR="$HERE/regression"
BIN="${NAVGATOR_BIN:-$(cd "$HERE/../../.." && pwd)/target/debug/navgator}"
DISP="${REG_DISPLAY:-:99}"
W=900; H=620; PORT="${REG_PORT:-8866}"
SSIM_MIN="${REG_SSIM_MIN:-0.92}"
fail=0

[ -x "$BIN" ] || { echo "navgator binary not found at $BIN (build it first)"; exit 2; }

DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1 || {
  setsid Xvfb "$DISP" -screen 0 ${W}x${H}x24 +extension GLX +render -noreset >/tmp/reg-xvfb.log 2>&1 < /dev/null &
  sleep 3; }
( cd "$DIR" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 < /dev/null & ); sleep 1
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-/tmp/navgator-run/profile}"; mkdir -p "$XDG_CONFIG_HOME"
TOP=78  # navgator chrome toolbar height; content is painted below it

render() { # <name> <page.html>  -> /tmp/reg_<name>_c.png (content region, toolbar cropped)
  local name="$1" page="$2"
  setsid env DISPLAY="$DISP" "$BIN" "http://localhost:$PORT/$page" >/dev/null 2>&1 < /dev/null &
  local pid=$!; disown 2>/dev/null; sleep 7
  ffmpeg -y -draw_mouse 0 -f x11grab -video_size ${W}x${H} -i "${DISP}.0" -frames:v 1 "/tmp/reg_$name.png" >/dev/null 2>&1
  { kill -9 "$pid"; pkill -9 -P "$pid"; } 2>/dev/null
  ffmpeg -y -i "/tmp/reg_$name.png" -vf "crop=${W}:$((H-TOP)):0:$TOP" "/tmp/reg_${name}_c.png" >/dev/null 2>&1
}
ssim() { ffmpeg -i "$1" -i "$2" -lavfi ssim -f null - 2>&1 | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2; }

# --- self-reftests: <name> renders <name>.test.html, compared to <name>.ref.html ---
for t in mask_circle mask_chevron scheme_light; do
  render "${t}_t" "${t}.test.html"
  render "${t}_r" "${t}.ref.html"
  s=$(ssim "/tmp/reg_${t}_t_c.png" "/tmp/reg_${t}_r_c.png")
  ok=$(awk -v s="${s:-0}" -v m="$SSIM_MIN" 'BEGIN{print (s>=m)?"PASS":"FAIL"}')
  printf '[%s] %-14s SSIM=%-9s (>= %s)\n' "$ok" "$t" "${s:-NA}" "$SSIM_MIN"
  [ "$ok" = FAIL ] && fail=1
done

# --- color assertion: form-control accent (#007aff) must be present (not grey/black) ---
render forms "forms_accent.html"
blue=$(python3 - "/tmp/reg_forms_c.png" <<'PY'
import sys, warnings
warnings.filterwarnings("ignore")
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
# count accent-blue pixels (high blue, clearly bluer than red/green; excludes white & grey)
n = sum(1 for r, g, b in im.getdata() if b > 150 and b > r + 60 and b > g + 30)
print(n)
PY
)
ok=$([ "${blue:-0}" -gt 300 ] && echo PASS || echo FAIL)
printf '[%s] %-14s accent-blue px=%-7s (> 300)\n' "$ok" "forms_accent" "${blue:-0}"
[ "$ok" = FAIL ] && fail=1

# cleanup the per-run http server (leave Xvfb for reuse)
hp=$(ps -eo pid,args 2>/dev/null | awk -v p="http.server $PORT" '$0 ~ p && !/awk/{print $1}')
[ -n "$hp" ] && kill -9 $hp 2>/dev/null

echo "----"
[ "$fail" = 0 ] && echo "REGRESSION: all passed" || echo "REGRESSION: FAILURES detected"
exit $fail
