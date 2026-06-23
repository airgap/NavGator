#!/usr/bin/env bash
# Regenerate the app icons from the gator artwork.
#   in : packaging/navgator.icon/Assets/gator3.png   (gator line-art on opaque white)
#   out: packaging/navgator.icns           (macOS .app — CFBundleIconFile=navgator)
#        packaging/navgator.png  (256x256)  (Linux .desktop / AppImage / .DirIcon)
#        packaging/navgator.ico             (Windows .exe resource)
#
# Cross-platform rounding: Windows, Linux, and pre-Tahoe macOS do NOT round app
# icons, so the rounded-square shape is BAKED into a FULL-CANVAS bitmap — the
# white tile fills the whole 1024 canvas, only the corners are transparent, and
# there is NO inset margin and NO drop shadow. macOS Tahoe+ masks every icon to
# its own squircle; because the art reaches the rounded edge (no margin), that
# re-mask aligns with our baked radius and never exposes the grey backing — which
# is what produced the old "white squircle inside a grey squircle" bug. The art's
# own off-center framing is preserved (we only fit + pad, never recentre).
# Run on macOS after changing the art, then commit the regenerated icns/png/ico.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ART="$ROOT/packaging/navgator.icon/Assets/gator3.png"
RADIUS=230   # ~22.5% of 1024 — close to Apple's squircle so Tahoe's re-mask aligns

command -v magick   >/dev/null || { echo "needs ImageMagick (brew install imagemagick)"; exit 1; }
command -v iconutil >/dev/null || { echo "needs iconutil (macOS)"; exit 1; }
[ -f "$ART" ] || { echo "missing icon art: $ART"; exit 1; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# Full-canvas rounded mask (corners transparent, everything else opaque).
magick -size 1024x1024 xc:none -fill white \
  -draw "roundrectangle 0,0,1023,1023,$RADIUS,$RADIUS" "$TMP/mask.png"
# Art on white, fit to the canvas and padded to square — framing left as-authored.
magick "$ART" -strip -resize 1024x1024 -background white -gravity center -extent 1024x1024 "$TMP/flat.png"
# Clip the white art to the rounded shape so its square corners can't un-round it.
magick "$TMP/flat.png" \( "$TMP/mask.png" -alpha extract \) \
  -compose CopyOpacity -composite -define png:color-type=6 "$TMP/master.png"

# macOS .icns
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

echo "wrote packaging/navgator.icns, packaging/navgator.png, packaging/navgator.ico"
