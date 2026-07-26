# SVG parity with Chrome (LYK-136)

Status of the inline-SVG parity program. Engine work lives in airgap/swervo
(PR #29, branch `nicole/lyk-136-svg-serializer-parity`); NavGator regression
coverage in `.claude/skills/run-navgator/regression/svg_*.html`.

## How swervo renders inline SVG

The `<svg>` subtree is XML-serialized to a standalone `data:image/svg+xml`
document (script: `svgsvgelement.rs::serialize_and_cache_subtree`), loaded
through the image cache, and rasterized by resvg at a size layout requests
in device pixels (`device_pixel_ratio` — includes page zoom — keyed per
(image, size), so multiple scales coexist). Anything that doesn't survive
that round trip does not render.

## Landed (stages 1–2, swervo bc69b3c)

| Gap | Fix | Gate |
|---|---|---|
| Cross-doc `url(#id)` masks/clips/filters dropped (square Discord status dots) | serializer clones referenced defs into the subtree, recursively | `svg_xref_mask` |
| `<foreignObject>` rendered nothing (missing avatars) | img-only foreignObject lowered to `<image>` with decoded raster as data:PNG | `svg_foreignobject` |
| external `<image href>` rendered nothing | SVGImageElement fetches via image cache (poster-frame pattern); serializer re-embeds as data:PNG | `svg_image_href` |
| deep DOM mutations froze at first paint | SVGElement mutation hooks invalidate enclosing svg roots' cached serialization | manual (700ms toggle → alternating frames) |

Gotchas encoded in the code: SVG attribute names are case-sensitive
(`preserveAspectRatio`) — the lowercase-asserting `set_attribute` /
`get_string_attribute` paths panic; use `set_attribute_from_parser` /
`get_attribute_string_value_with_namespace`.

## Deferred (known, low-value or optimization)

- fontconfig alias parity for SVG **text** (`fontdb.load_system_fonts()` is
  already wired; uninstalled-family substitution à la Arial→Liberation is not)
- re-serialize/re-raster throttling for animation-heavy svg (correct today,
  each mutation re-rasters; damage-scoped throttling is an optimization)
- mutations to an externally-referenced `<mask>` don't invalidate svgs that
  reference it (needs a reverse index; refs are static on real sites)
- `background-image`-based avatars inside foreignObject (no `<img>` to
  source pixels from)

## Stage 3 design: native foreignObject (not started)

Goal: arbitrary HTML inside `<foreignObject>` with real hit-testing and a11y —
the one thing serializer lowering can never do (resvg cannot lay out HTML).

Sketch, using machinery that already exists:

1. **Layout**: stop treating the svg's subtree as opaque. The foreignObject's
   HTML children are already in the DOM and styled by stylo. Build them as an
   absolutely-positioned independent formatting context whose containing block
   is the svg replaced box, positioned by the foreignObject's x/y/width/height
   mapped through the svg viewBox transform.
2. **Serialization**: suppress foreignObject content from the serialized
   document (keep lowering as fallback when native layout is off), so the
   raster and the native layer never double-paint.
3. **Compositing**: push the foreignObject fragment as a WR stacking context
   clipped by an image mask — `define_clip_image_mask` (landed for CSS
   `mask-image`, LYK-1246) — where the mask bitmap is the svg-side `<mask>`
   rasterized alone at the foreignObject's device size (the serializer's
   external-ref inlining already knows how to materialize that mask).
4. **Paint order**: the svg raster splits into below/above layers only when a
   foreignObject has svg siblings painted above it (Discord: status rect) —
   phase 2; phase 1 can paint native content always-on-top, which is correct
   for the avatar pattern.

Risks: viewBox↔CSS-px transform plumbing in layout; interaction of the WR
image-mask clip with scroll frames; invalidation coupling between the two
layers. Prototype behind an engine pref (`dom_svg_foreignobject_native`).
