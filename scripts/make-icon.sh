#!/usr/bin/env bash
# Regenerate the app icons from the Icon Composer source.
#   in : packaging/navgator.icon            (Icon Composer bundle; edit this)
#   out: packaging/navgator.icns            (macOS .app — referenced by Info.plist)
#        packaging/navgator.png  (256x256)  (Linux .desktop / AppImage / .DirIcon)
#
# Why this exists: the official compositor (actool / Icon Composer.app) needs full
# Xcode. This reproduces the navgator.icon design — the gator line-art layer on a
# white rounded tile with a soft neutral shadow (see navgator.icon/icon.json) —
# with ImageMagick + iconutil, which ship with the Command Line Tools. Run on
# macOS after editing the .icon, then commit the regenerated icns/png.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICON="$ROOT/packaging/navgator.icon"
ART="$ICON/Assets/gator2.png"

command -v magick   >/dev/null || { echo "needs ImageMagick (brew install imagemagick)"; exit 1; }
command -v iconutil >/dev/null || { echo "needs iconutil (macOS)"; exit 1; }
[ -f "$ART" ] || { echo "missing icon art: $ART"; exit 1; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# 1024 master: white rounded tile (macOS icon grid ~100px margin) + gator art + soft shadow.
magick -size 824x824 xc:none -fill white -draw "roundrectangle 0,0,823,823,185,185" "$TMP/tile.png"
magick "$ART" -resize x620 "$TMP/art.png"
magick "$TMP/tile.png" "$TMP/art.png" -gravity center -composite "$TMP/tiled.png"
magick "$TMP/tiled.png" \( +clone -background black -shadow 50x18+0+16 \) +swap \
  -background none -layers merge +repage "$TMP/shadowed.png"
magick -size 1024x1024 xc:none "$TMP/shadowed.png" -gravity center -geometry +0-6 \
  -composite -define png:color-type=6 "$TMP/master.png"

# macOS .icns
SET="$TMP/navgator.iconset"; mkdir -p "$SET"
for s in 16 32 128 256 512; do
  magick "$TMP/master.png" -resize "${s}x${s}"         "$SET/icon_${s}x${s}.png"
  magick "$TMP/master.png" -resize "$((s*2))x$((s*2))" "$SET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$SET" -o "$ROOT/packaging/navgator.icns"

# Linux 256x256 PNG (matches hicolor/256x256/apps in scripts/package.sh)
magick "$TMP/master.png" -resize 256x256 -define png:color-type=6 "$ROOT/packaging/navgator.png"

echo "wrote packaging/navgator.icns and packaging/navgator.png"
