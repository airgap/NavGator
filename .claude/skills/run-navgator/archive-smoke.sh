#!/usr/bin/env bash
# Verify the record/replay archive: record a real page, then replay it twice and confirm the two
# replays are pixel-identical (deterministic) and that replay #2 needed no network (misses logged).
set -uo pipefail
W=1280; H=800; VH=722; TOP=78; DISP=:99
BIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)/target/debug/navgator"
SITE="${1:-https://news.ycombinator.com/}"
DIR=/tmp/arc/store; rm -rf "$DIR"
OUT=/tmp/arc; mkdir -p "$OUT"
export XDG_CONFIG_HOME=/tmp/arc/profile; mkdir -p "$XDG_CONFIG_HOME"

DISPLAY=$DISP xdpyinfo >/dev/null 2>&1 || { setsid Xvfb $DISP -screen 0 ${W}x${H}x24 +extension GLX +render -noreset >/tmp/arc/xvfb.log 2>&1 < /dev/null & sleep 3; }

render(){ # <mode> <out-name> <settle>
  setsid env DISPLAY=$DISP NAVGATOR_ARCHIVE_DIR="$DIR" NAVGATOR_ARCHIVE_MODE="$1" \
    "$BIN" "$SITE" >"/tmp/arc/log_$2.txt" 2>&1 < /dev/null & local pid=$!
  disown 2>/dev/null; sleep "$3"
  ffmpeg -y -draw_mouse 0 -f x11grab -video_size ${W}x${H} -i ${DISP}.0 -frames:v 1 "$OUT/full_$2.png" >/dev/null 2>&1
  ffmpeg -y -i "$OUT/full_$2.png" -vf "crop=${W}:${VH}:0:${TOP}" "$OUT/$2.png" >/dev/null 2>&1
  { kill -9 "$pid"; pkill -9 -P "$pid"; } 2>/dev/null; sleep 0.5
}
ssim(){ ffmpeg -i "$1" -i "$2" -lavfi ssim -f null - 2>&1 | grep -oE 'All:[0-9.]+' | tail -1; }

echo "[1] RECORD $SITE"
render record record 14
echo "    archived: $(ls "$DIR"/*.json 2>/dev/null | wc -l) resources, $(du -sh "$DIR" 2>/dev/null | cut -f1)"

echo "[2] REPLAY (1st)"
render replay replay1 9
echo "[3] REPLAY (2nd)"
render replay replay2 9

echo "----"
echo "record vs replay1 SSIM: $(ssim "$OUT/record.png" "$OUT/replay1.png")"
echo "replay1 vs replay2 SSIM: $(ssim "$OUT/replay1.png" "$OUT/replay2.png")  (expect ~1.0 = deterministic)"
echo "replay misses: $(wc -l < "$DIR/misses.txt" 2>/dev/null || echo 0)  (lines in misses.txt)"
echo "screens: $OUT/{record,replay1,replay2}.png"
