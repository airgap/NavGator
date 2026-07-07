#!/usr/bin/env python3
"""Overlay a build label (e.g. "0.1.0-221") on the app icon at package time.

NavGator is in active development, so every packaged build stamps its
`{version}-{commit-count}` into a small badge in the top-right of the app icon,
making builds trivially distinguishable in the Dock / Finder / task bar.

This runs from scripts/package.sh and emits *stamped* icon files into an output
dir WITHOUT touching the committed clean icons in packaging/. It produces:

  <out>/navgator.iconset/   the full iconset (iconutil packs it into .icns on macOS)
  <out>/navgator.png        256x256 (Linux .desktop / AppImage / .DirIcon)
  <out>/navgator.ico        Windows multi-res (harmless if unused)
  <out>/navgator.icon/      a copy of the Icon Composer bundle with a badge LAYER
                            added, so `actool` bakes the badge into the Tahoe
                            Assets.car too (the gator layer recolors its art to a
                            flat tint, so the badge can't live in that layer — it
                            goes in its own no-fill layer that keeps its colors).

The master is rebuilt from the source art with the SAME fit+pad+round parameters
as scripts/make-icon.sh (keep them in sync); we don't upscale the committed 256px
png. Requires Pillow; package.sh degrades to the clean committed icons if it or
this script fails, so a build without Pillow still ships (just unstamped).
"""
import argparse
import json
import os
import shutil
import sys

try:
    from PIL import Image, ImageChops, ImageDraw, ImageFont
except Exception as e:  # pragma: no cover - exercised via package.sh degrade path
    sys.stderr.write(f"stamp-icon: Pillow unavailable ({e}); skipping stamp\n")
    sys.exit(2)

CANVAS = 1024
RADIUS = 230  # ~22.5% of 1024, matches make-icon.sh
ICONSET_SIZES = [16, 32, 128, 256, 512]  # each also emitted @2x
BADGE_RGBA = (214, 40, 64, 255)          # dev crimson
BADGE_OUTLINE = (255, 255, 255, 235)
BADGE_TEXT = (255, 255, 255, 255)

FONT_CANDIDATES = [
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",   # macOS
    "/Library/Fonts/Arial Bold.ttf",                        # macOS (older)
    "/System/Library/Fonts/Helvetica.ttc",                  # macOS
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", # Linux
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
]


def load_font(size):
    for path in FONT_CANDIDATES:
        if os.path.exists(path):
            try:
                return ImageFont.truetype(path, size)
            except Exception:
                continue
    return ImageFont.load_default()


def rounded_master(art_path):
    """Build the white 1024 rounded-square master with the gator rising from the BOTTOM
    edge (its design intent), with no padding — the same master scripts/make-icon.sh builds
    (kept intentionally in sync).

    The source art is a small (735x824) gator drawn on white with a lot of headroom above
    it and its feet already flush to the art's bottom edge. The old code thumbnail'd (which
    never upscales) and centred it, so the gator ended up inset with ~100px of padding on
    every side. Instead: trim the white margins to the gator's true bounds, scale it to
    fill the canvas WIDTH, and anchor it to the bottom edge — any height overflow is cropped
    off the TOP (white headroom only), so the gator reaches the bottom with zero padding."""
    art = Image.open(art_path).convert("RGBA")
    # Trim white margins to the gator's true content box.
    bbox = ImageChops.difference(
        art.convert("RGB"), Image.new("RGB", art.size, (255, 255, 255))
    ).getbbox()
    if bbox:
        art = art.crop(bbox)
    scale = CANVAS / art.width
    gator = art.resize((CANVAS, round(art.height * scale)), Image.LANCZOS)
    if gator.height > CANVAS:  # overflow is the headroom above the gator — crop it off the top
        gator = gator.crop((0, gator.height - CANVAS, CANVAS, gator.height))
    flat = Image.new("RGBA", (CANVAS, CANVAS), (255, 255, 255, 255))
    flat.alpha_composite(gator, (0, CANVAS - gator.height))  # anchor to the bottom edge
    mask = Image.new("L", (CANVAS, CANVAS), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, CANVAS - 1, CANVAS - 1], radius=RADIUS, fill=255)
    flat.putalpha(mask)
    return flat


def badge_layer(label):
    """A full-canvas transparent image carrying only the top-right badge pill."""
    layer = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    font = load_font(int(CANVAS * 0.085))
    tb = d.textbbox((0, 0), label, font=font)
    tw, th = tb[2] - tb[0], tb[3] - tb[1]
    padx, pady = int(CANVAS * 0.035), int(CANVAS * 0.028)
    pw, ph = tw + 2 * padx, th + 2 * pady
    margin = int(CANVAS * 0.045)
    x1, y1 = CANVAS - margin - pw, margin
    d.rounded_rectangle([x1, y1, CANVAS - margin, y1 + ph], radius=ph // 2,
                        fill=BADGE_RGBA, outline=BADGE_OUTLINE, width=max(2, CANVAS // 220))
    d.text((x1 + padx - tb[0], y1 + pady - tb[1]), label, font=font, fill=BADGE_TEXT)
    return layer


def stamp_icon_bundle(src_bundle, out_bundle, badge_png_name):
    """Copy the Icon Composer bundle and add a badge layer that keeps its own
    colors (no fill-specialization = actool renders the image as-is)."""
    shutil.copytree(src_bundle, out_bundle, dirs_exist_ok=True)
    icon_json = os.path.join(out_bundle, "icon.json")
    with open(icon_json) as f:
        spec = json.load(f)
    badge_layer_spec = {
        "image-name": badge_png_name,
        "name": "build-badge",
        "position": {"scale": 1.0, "translation-in-points": [0, 0]},
    }
    groups = spec.setdefault("groups", [])
    if groups:
        groups[-1].setdefault("layers", []).append(badge_layer_spec)
    else:
        groups.append({"layers": [badge_layer_spec]})
    with open(icon_json, "w") as f:
        json.dump(spec, f, indent=2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True, help='e.g. "0.1.0-221"')
    ap.add_argument("--art", required=True, help="source art PNG (packaging/navgator.icon/Assets/gator3.png)")
    ap.add_argument("--icon-bundle", help="Icon Composer .icon bundle to stamp for actool (optional)")
    ap.add_argument("--out-dir", required=True)
    args = ap.parse_args()

    os.makedirs(args.out_dir, exist_ok=True)
    master = rounded_master(args.art)
    stamped = master.copy()
    stamped.alpha_composite(badge_layer(args.label))

    # iconset for iconutil -> .icns
    iconset = os.path.join(args.out_dir, "navgator.iconset")
    os.makedirs(iconset, exist_ok=True)
    for s in ICONSET_SIZES:
        stamped.resize((s, s), Image.LANCZOS).save(os.path.join(iconset, f"icon_{s}x{s}.png"))
        stamped.resize((s * 2, s * 2), Image.LANCZOS).save(os.path.join(iconset, f"icon_{s}x{s}@2x.png"))

    # Linux 256 png + Windows ico
    stamped.resize((256, 256), Image.LANCZOS).save(os.path.join(args.out_dir, "navgator.png"))
    stamped.save(os.path.join(args.out_dir, "navgator.ico"),
                 sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])

    # Tahoe .icon bundle: badge as its own layer
    if args.icon_bundle and os.path.isdir(args.icon_bundle):
        out_bundle = os.path.join(args.out_dir, "navgator.icon")
        badge_png_name = "build-badge.png"
        stamp_icon_bundle(args.icon_bundle, out_bundle, badge_png_name)
        badge_layer(args.label).save(os.path.join(out_bundle, "Assets", badge_png_name))

    sys.stderr.write(f"stamp-icon: wrote stamped icons for '{args.label}' to {args.out_dir}\n")


if __name__ == "__main__":
    main()
