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
# SSIM alone is too forgiving here: a test page missing its whole 120px subject still scored
# 0.99 (the subject is ~2% of the frame). Also cap the share of pixels that differ outright;
# real passes stay under 0.25% (anti-aliasing at mask edges), a missing or recoloured shape
# lands around 1-2%.
DIFF_MAX="${REG_DIFF_MAX:-0.5}"
fail=0

[ -x "$BIN" ] || { echo "navgator binary not found at $BIN (build it first)"; exit 2; }
# Measured before this suite starts its own (smaller) Xvfb on the same display.
TOP="$(NAVG_BIN="$BIN" "$HERE/driver.sh" chrome-top)" || exit 1  # content is painted below it

xvfb_pid=""
DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1 || {
  setsid Xvfb "$DISP" -screen 0 ${W}x${H}x24 +extension GLX +render -noreset >/tmp/reg-xvfb.log 2>&1 < /dev/null &
  xvfb_pid=$!
  sleep 3; }
( cd "$DIR" && setsid python3 -m http.server "$PORT" >/dev/null 2>&1 < /dev/null & ); sleep 1
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-/tmp/navgator-run/profile}"; mkdir -p "$XDG_CONFIG_HOME"

render() { # <name> <page.html>  -> /tmp/reg_<name>_c.png (content region, toolbar cropped)
  local name="$1" page="$2"
  setsid env DISPLAY="$DISP" "$BIN" "http://localhost:$PORT/$page" >/dev/null 2>&1 < /dev/null &
  local pid=$!; disown 2>/dev/null; sleep 7
  ffmpeg -y -draw_mouse 0 -f x11grab -video_size ${W}x${H} -i "${DISP}.0" -frames:v 1 "/tmp/reg_$name.png" >/dev/null 2>&1
  { kill -9 "$pid"; pkill -9 -P "$pid"; } 2>/dev/null
  ffmpeg -y -i "/tmp/reg_$name.png" -vf "crop=${W}:$((H-TOP)):0:$TOP" "/tmp/reg_${name}_c.png" >/dev/null 2>&1
}
ssim() { ffmpeg -i "$1" -i "$2" -lavfi ssim -f null - 2>&1 | grep -oE 'All:[0-9.]+' | tail -1 | cut -d: -f2; }
diffpct() { # % of pixels where some channel differs by more than 48
  python3 - "$1" "$2" <<'PY'
import sys, warnings
warnings.filterwarnings("ignore")
from PIL import Image, ImageChops
a = Image.open(sys.argv[1]).convert("RGB"); b = Image.open(sys.argv[2]).convert("RGB")
n = sum(1 for px in ImageChops.difference(a, b).getdata() if max(px) > 48)
print(f"{n * 100 / (a.size[0] * a.size[1]):.3f}")
PY
}

# --- self-reftests: <name> renders <name>.test.html, compared to <name>.ref.html ---
# svg_xref_mask + svg_foreignobject need the LYK-136 serializer passes (cross-doc ref
# inlining + foreignObject lowering) — red on an engine pin older than that merge.
# svg_css_paint needs computed paint properties carried into the serialization (and stylo's
# fill/stroke family enabled for servo); without them every icon on it paints black.
# svg_paint_restyle changes those properties after first paint; the cached serialization must
# be rebuilt (layout compares a paint signature), or the icons keep their first colours.
# svg_fo_in_group needs the native-foreignObject widget to look through svg containers.
for t in mask_circle mask_chevron scheme_light clip_text grid_cols light_dark svg_xref_mask svg_foreignobject svg_image_href svg_fo_use_mask svg_css_paint svg_paint_restyle svg_fo_in_group; do
  render "${t}_t" "${t}.test.html"
  render "${t}_r" "${t}.ref.html"
  s=$(ssim "/tmp/reg_${t}_t_c.png" "/tmp/reg_${t}_r_c.png")
  d=$(diffpct "/tmp/reg_${t}_t_c.png" "/tmp/reg_${t}_r_c.png")
  ok=$(awk -v s="${s:-0}" -v m="$SSIM_MIN" -v d="${d:-100}" -v x="$DIFF_MAX" \
    'BEGIN{print (s>=m && d<=x)?"PASS":"FAIL"}')
  printf '[%s] %-17s SSIM=%-9s (>= %s)  diff=%s%% (<= %s%%)\n' "$ok" "$t" "${s:-NA}" "$SSIM_MIN" "${d:-NA}" "$DIFF_MAX"
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

# cleanup the per-run http server, and the Xvfb if this run started it: left behind at this
# suite's 900x620, the driver would reuse it and its 1280x800 captures would silently fail.
hp=$(ps -eo pid,args 2>/dev/null | awk -v p="http.server $PORT" '$0 ~ p && !/awk/{print $1}')
[ -n "$hp" ] && kill -9 $hp 2>/dev/null
[ -n "$xvfb_pid" ] && kill -9 "$xvfb_pid" 2>/dev/null

echo "----"
[ "$fail" = 0 ] && echo "REGRESSION: all passed" || echo "REGRESSION: FAILURES detected"
exit $fail
