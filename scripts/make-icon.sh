#!/usr/bin/env bash
# Regenerate the raster app icons from the gator artwork.
#   in : packaging/navgator.icon/Assets/gator3.png   (gator line-art on opaque white)
#   out: packaging/navgator.icns           (macOS CFBundleIconFile — PRE-Tahoe fallback)
#        packaging/navgator.png  (256x256)  (Linux .desktop / AppImage / .DirIcon)
#        packaging/navgator.ico             (Windows .exe resource)
#
# macOS Tahoe (26+) does NOT use this .icns. It reads CFBundleIconName -> an
# AppIcon baked into a compiled Assets.car, which scripts/package.sh builds from
# packaging/navgator.icon with actool (full Xcode only — Command Line Tools lack
# it, so local CLT builds fall back to this .icns and Tahoe shows it on its grey
# "squircle jail" plate). A raster .icns can never escape that plate on Tahoe.
# Windows/Linux/pre-Tahoe don't mask, so the rounding is baked into all three
# raster files here. The gator is trimmed of its white margins and anchored to the
# bottom edge (its design intent: it rises from the bottom with no padding), filling
# the width. Run on macOS after changing the art, then commit the regenerated files.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ART="$ROOT/packaging/navgator.icon/Assets/gator3.png"
RADIUS=230   # ~22.5% of 1024

command -v magick   >/dev/null || { echo "needs ImageMagick (brew install imagemagick)"; exit 1; }
command -v iconutil >/dev/null || { echo "needs iconutil (macOS)"; exit 1; }
[ -f "$ART" ] || { echo "missing icon art: $ART"; exit 1; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# Full-canvas rounded master (white fills the canvas, only the corners transparent).
magick -size 1024x1024 xc:none -fill white \
  -draw "roundrectangle 0,0,1023,1023,$RADIUS,$RADIUS" "$TMP/mask.png"
# Trim the art's white margins to the gator's true bounds, scale it to fill the width, and
# anchor it to the BOTTOM edge (gravity south) so the gator rises from the bottom with no
# padding (its design intent). -extent crops any height overflow off the top (headroom only)
# and pads the top of short art with white. Keep in sync with scripts/stamp-icon.py.
magick "$ART" -strip -fuzz 2% -trim +repage -resize 1024x -background white -gravity south -extent 1024x1024 "$TMP/flat.png"
magick "$TMP/flat.png" \( "$TMP/mask.png" -alpha extract \) \
  -compose CopyOpacity -composite -define png:color-type=6 "$TMP/master.png"

# macOS .icns (pre-Tahoe fallback)
SET="$TMP/navgator.iconset"; mkdir -p "$SET"
for s in 16 32 128 256 512; do
  magick "$TMP/master.png" -resize "${s}x${s}"         "$SET/icon_${s}x${s}.png"
  magick "$TMP/master.png" -resize "$((s*2))x$((s*2))" "$SET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$SET" -o "$ROOT/packaging/navgator.icns"

# Linux 256x256 PNG (matches hicolor/256x256/apps in scripts/package.sh)
magick "$TMP/master.png" -resize 256x256 -define png:color-type=6 "$ROOT/packaging/navgator.png"

# Windows multi-resolution .ico
magick "$TMP/master.png" -define icon:auto-resize=16,32,48,64,128,256 "$ROOT/packaging/navgator.ico"

echo "wrote packaging/navgator.icns (pre-Tahoe), packaging/navgator.png, packaging/navgator.ico"
