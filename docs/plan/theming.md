# swerve theming & customization

> Dimension owner doc. The headline differentiator: **Opera GX-class deep theming, but
> fast.** This document is grounded in the actual swerve code (`src/main.rs`,
> `src/chrome/*`) and in the pinned Servo source at rev `ed1af70` (cached at
> `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`) and its stylo
> (`stylo-482338307e42a9ea/49e912c`). Every Servo capability claim below was verified by
> reading that source, not assumed.

---

## 0. TL;DR

- **The chrome is already an HTML document Servo paints** (`src/chrome/index.html` +
  `chrome.css` + `chrome.js`). That is the entire theming substrate. Theming the chrome =
  swapping CSS custom properties / stylesheets in a document we own. This is *cheap* and
  the single biggest lever — most of GX's look is achievable with CSS variables + a
  hot-reload path. No Servo changes required for chrome theming.
- **Content theming (force-dark, per-site CSS) is real and supported.** Servo exposes a
  public `UserContentManager` API (`servo::UserContentManager`, verified in
  `components/servo/user_content_manager.rs` and exercised by Servo's own test
  `components/servo/tests/user_content_manager.rs`) with `add_stylesheet` / `add_script` /
  `remove_*`. User stylesheets cascade at `Origin::User`. **Plus** `WebView::notify_theme_change(Theme::Dark)`
  flips `prefers-color-scheme` *live, no reload* (verified by Servo test `test_theme_change`).
- **Three hard Servo constraints shape the whole design:**
  1. `UserContentManager` updates "take effect only after the page is reloaded" (their
     doc comment, verified). So injected CSS is **not** hot on the live page — only
     `notify_theme_change` and `chrome.evaluate_javascript` are hot.
  2. `@-moz-document` (the userstyles.org / Stylus per-site targeting mechanism) is
     `cfg!(feature = "gecko")` and **disabled in Servo** (verified in stylo
     `stylesheets/rule_parser.rs:779`). Per-site targeting must be done by the *embedder*
     (attach the right sheet to the right WebView by URL), not by `@-moz-document` inside
     the CSS.
  3. `backdrop-filter` is **not wired to the display list** (verified: no display-item
     build in `components/layout/display_list/`; only a UA-css mention + `::backdrop`
     FIXME). The frosted-glass GX aesthetic will not render. `filter:` (blur, brightness,
     etc.) *does* work. Plan the visual language around `filter`, gradients, shadows — not
     `backdrop-filter`.
- **Performance is the differentiator, not the feature list.** GX is widely criticised for
  650 MB–1.2 GB RAM and 80–100% CPU spikes ([Opera forums][gx-cpu], [techysnoop][gx-mem]).
  swerve's whole pitch is "the customization without the cost." That means a hard,
  measured perf budget on the theming layer specifically (the chrome must stay a near-zero
  idle-cost document).

---

## 1. What "deep theming" must cover (scope)

| Area | What users expect (GX / Firefox userChrome bar) | swerve feasibility |
| --- | --- | --- |
| Light / dark / custom | base palettes + a true custom palette | **Easy** — chrome CSS tokens; content via `notify_theme_change` |
| Accent color | one knob recolors the whole UI | **Easy** — single `--accent` token + derived tokens via `color-mix()` |
| Wallpapers | omnibox/newtab/sidebar background image | **Easy (chrome)**; needs an asset protocol (see §6) |
| Fonts (UI) | pick chrome UI font + size | **Easy** — chrome `--font-ui` token; bundled fonts via `@font-face` (gated, see §5) |
| Fonts (content) | change page default font | **Hard / global** — `fonts.*` prefs are global + need reload (see §5) |
| Density / layout presets | compact/normal/spacious; tab position | **Medium** — token-driven spacing + chrome layout variants |
| Sounds | hover/click/open feedback (GX's signature) | **Medium** — `dom_webaudio`/`<audio>` in chrome; latency caveats (§7) |
| Motion | enable/reduce animations | **Easy** — CSS transitions/`@keyframes` work; a `--motion` gate token |
| Force-dark (content) | dark-mode any site | **Medium** — `notify_theme_change` + an injected force-dark user stylesheet (§4) |
| Per-site themes (content) | restyle specific sites | **Medium-Hard** — embedder-driven per-WebView sheets, no `@-moz-document` (§4) |
| Mods (chrome behavior) | add buttons, panels, rework UI | **Medium** — a sandboxed chrome-mod API over the existing bridge (§8) |
| Marketplace / sync | install + sync themes via Lyku | **Medium** — package format + signature + Lyku blobs (§9, §10) |

---

## 2. Current state (verified facts about the substrate)

From `src/main.rs` + `src/chrome/*`:

- The chrome is a Servo `WebView` loaded from a `file://` URL (`chrome_url()` →
  `src/chrome/index.html`). It already uses CSS custom properties under `:root`
  (`--bg`, `--bar`, `--accent`, `--radius`, …) — i.e. **a token system already exists**, it
  just isn't formalized, themeable, or persisted.
- The engine→chrome push channel is `WebView::evaluate_javascript(js, cb)`
  (`AppState::chrome_eval`, used by `push_model`). The chrome→engine channel is a denied
  navigation to a `swerve:` URL intercepted in `request_navigation`. **Both channels are
  in place and are exactly what a theme/mod system needs.** A theme apply is just a new
  `swerve:` verb + a `swerve:state`-style event.
- `chrome.css` already documents Servo gaps it had to work around:
  `user-select` is an inert stub (worked around via `selectstart` in JS), and
  `text-overflow: ellipsis` is unimplemented (worked around with a JS binary-search
  truncator). **These are the kind of small Servo gaps the theme system must assume and
  budget for.** Treat the chrome stylesheet as "CSS that must run on Servo," not "CSS that
  must run on Chrome."

What does *not* exist yet: any notion of a theme as data, persistence, hot-reload, a
package format, a mod API, force-dark, per-site styling, or sync. Everything in this doc
is greenfield on top of the substrate above.

### 2.1 Servo capability matrix (verified at rev `ed1af70` / stylo `49e912c`)

| Capability | Status at pinned rev | Evidence |
| --- | --- | --- |
| CSS custom properties (`var()`) | **Yes** (already used in chrome.css) | `stylo/custom_properties.rs` |
| `color-mix()` (token derivation) | **Yes** | `stylo/values/specified/color.rs`; n-color support added in Servo 0.2.0 (Apr 2026) |
| `@property` registered custom props | **Parsed** | `stylo/stylesheets/property_rule.rs` |
| `@layer` cascade layers | **Parsed** | `stylo/stylesheets/layer_rule.rs` |
| `:has()` / `:is()` / `:where()` | **Yes** | selectors crate; Servo blog Dec 2024 |
| Flexbox | **Yes, ungated** (chrome relies on it) | not in pref gate list |
| CSS Grid | **Implemented but OFF by default** | `layout_grid_enabled: false` (`config/prefs.rs:515`) |
| Container queries | **OFF by default** | `layout_container_queries_enabled: false` |
| Multicol (`columns`) | **OFF by default** | `layout_columns_enabled: false` |
| Variable fonts | **OFF by default** (landed Sept 2025) | `layout_variable_fonts_enabled: false` |
| `@font-face` (web/bundled fonts) | **OFF by default** | `dom_fontface_enabled: false` |
| `adoptedStyleSheets` | **OFF by default** | `dom_adoptedstylesheet_enabled: false` |
| `ResizeObserver` | **ON** | `dom_resize_observer_enabled: true` |
| `IntersectionObserver` | **OFF by default** | `dom_intersection_observer_enabled: false` |
| CSS transitions + `@keyframes` | **Yes, driven** | `layout/layout_impl.rs` animation timeline; `stylo/.../keyframes_rule.rs` |
| `filter:` (blur/brightness/…/drop-shadow) | **Yes, wired to WebRender FilterOp** | `layout/display_list/conversions.rs:31-41` |
| `backdrop-filter` | **NOT wired** (won't render) | absent from `display_list/`; UA-css + `::backdrop` FIXME only |
| Gradients, `box-shadow`, `border-radius` | **Yes** | `layout/display_list/gradient.rs`, `mod.rs` |
| `prefers-color-scheme` live switch | **Yes, hot** | `WebView::notify_theme_change` + test `test_theme_change` |
| `user-select` | **Inert stub** | noted in chrome.css; worked around in chrome.js |
| `text-overflow: ellipsis` | **Unimplemented** | noted in chrome.css |
| Custom URL scheme (asset serving) | **Yes** (`ServoBuilder::protocol_registry`) | `net/protocols/mod.rs:104 register` |
| Runtime pref set | **Yes** `Servo::set_preference(name, val)` | `components/servo/servo.rs:1082` |

**Action implied by this table:** to use grid layout, web/bundled fonts (`@font-face`), or
variable fonts *in the chrome*, the embedder must turn the corresponding prefs ON at
`ServoBuilder::preferences(...)`. The chrome today survives on flexbox + system fonts;
the theme engine should keep flexbox as the layout baseline (always available) and treat
grid as opt-in only after we enable + verify it.

---

## 3. Theme model (design tokens)

### 3.1 Principle: one token layer, two consumers

Define a single flat token namespace. The **chrome** consumes tokens as CSS custom
properties on `:root`. The **content** force-dark/per-site layer consumes a *subset* of
the same tokens (compiled into an injected user stylesheet). One source of truth → two
render targets. This is what makes "change accent once, the whole browser follows."

Tokens are organized in tiers so a theme author can override at any altitude:

1. **Primitive** — raw values, rarely themed directly (`--sw-blue-500: #5b8cff`).
2. **Semantic** — what the UI references (`--sw-color-accent`, `--sw-color-bg`,
   `--sw-color-surface`, `--sw-color-text`, `--sw-color-text-dim`, `--sw-color-border`).
   Most are *derived* from a small set of inputs via `color-mix()` so a theme can ship 4
   colors and get 30.
3. **Component** — optional fine overrides (`--sw-tab-active-bg`). Default to the semantic
   tier; only present when an author wants pixel control.

### 3.2 The token catalog (initial v1)

```css
:root {
  /* ---- inputs a theme typically sets (the "knobs") ---- */
  --sw-scheme: dark;                 /* light | dark — also drives content notify_theme_change */
  --sw-color-accent: #5b8cff;
  --sw-color-bg: #1b1d21;            /* window base */
  --sw-color-surface: #25282d;       /* toolbar / tab strip */
  --sw-color-text: #e6e8eb;
  --sw-font-ui: system-ui, -apple-system, "Segoe UI", sans-serif;
  --sw-font-size: 13px;
  --sw-density: 1;                   /* 0.85 compact · 1 normal · 1.2 spacious */
  --sw-radius: 9px;
  --sw-motion: 1;                    /* 0 = reduce motion (gates transition durations) */
  --sw-wallpaper: none;              /* url(swerve-asset://theme/<id>/bg.webp) or none */
  --sw-wallpaper-opacity: 0.12;

  /* ---- derived (theme rarely touches these) ---- */
  --sw-color-surface-hi: color-mix(in srgb, var(--sw-color-surface) 80%, var(--sw-color-text) 20%);
  --sw-color-border:     color-mix(in srgb, var(--sw-color-surface) 70%, var(--sw-color-text) 30%);
  --sw-color-text-dim:   color-mix(in srgb, var(--sw-color-text) 60%, var(--sw-color-bg) 40%);
  --sw-color-accent-hover: color-mix(in srgb, var(--sw-color-accent) 85%, white 15%);
  --sw-tab-active-bg:    color-mix(in srgb, var(--sw-color-surface) 70%, var(--sw-color-bg) 30%);

  /* ---- spacing scale, density-aware ---- */
  --sw-space-1: calc(4px * var(--sw-density));
  --sw-space-2: calc(8px * var(--sw-density));
  --sw-space-3: calc(12px * var(--sw-density));
  --sw-toolbar-h: calc(46px * var(--sw-density));

  /* ---- motion ---- */
  --sw-dur-fast: calc(120ms * var(--sw-motion));
  --sw-dur-slow: calc(240ms * var(--sw-motion));
}
```

`color-mix()` is verified present in the pinned stylo, so this derivation works **today**.
`--sw-density` and `--sw-motion` as numeric multipliers in `calc()` is the trick that
turns "density preset" and "reduce motion" into a single number each, no class toggling.
(Note: `prefers-reduced-motion` exists as a concept but we drive motion ourselves via the
token so it is user-controllable, not OS-controlled.)

The existing `chrome.css` should be refactored so every hard-coded value (`#1b1d21`,
`84px`, `0.12s`, etc.) references a token. That refactor is **prerequisite work** and is
the first concrete deliverable — it has value even before any theme UI exists.

### 3.3 How a theme is applied (the hot path)

A theme is fundamentally **a set of token values**. Applying it:

1. Engine receives "apply theme X" (from the settings UI, a `swerve:` command, IPC, or
   Lyku sync).
2. Engine pushes the resolved token map to the chrome via `evaluate_javascript`:
   ```js
   window.dispatchEvent(new CustomEvent('swerve:theme', { detail: { tokens: {...}, css: "..." } }));
   ```
3. The chrome's theme runtime sets each token on `document.documentElement.style`
   (`el.style.setProperty('--sw-color-accent', v)`). This is **instant** — it is a style
   recalc on one document, no reload. This is the hot-reload story for the chrome (§4).
4. If `--sw-scheme` changed, the engine *also* calls
   `content_webview.notify_theme_change(Theme::Dark|Light)` for the active tab(s) so
   content's `prefers-color-scheme` follows the chrome.

This is the whole reason the HTML-chrome architecture is a superpower for theming: the
theme apply is a CSS variable write, the cheapest possible operation.

---

## 4. Hot-reload

There are **two different hot-reload stories** because Servo treats them differently.

### 4.1 Chrome hot-reload — fully hot, instant

- **Token changes:** push `swerve:theme` → `setProperty` on `:root`. Instant recalc. No
  reload, no flash. Verified path: `evaluate_javascript` already works (`push_model`).
- **Author CSS (a theme's extra rules / a mod's CSS):** inject/replace a known
  `<style id="sw-theme">` element's `textContent` from the `swerve:theme` payload's
  `css` field. Replacing a `<style>`'s text triggers a normal stylesheet swap on a live
  document — hot. We deliberately **avoid** `adoptedStyleSheets` because it is OFF by
  default at this rev (`dom_adoptedstylesheet_enabled: false`); a managed `<style>` tag is
  the portable mechanism.
- **Dev loop for theme authors:** watch the theme dir on disk (a Rust `notify` watcher in
  the engine), and on change re-read + re-push `swerve:theme`. Author edits `theme.css`,
  sees it in <100 ms without restarting swerve. This is the killer DX feature and it is
  cheap to build because it rides the existing bridge.

### 4.2 Content hot-reload — partially hot

| Change | Hot? | Mechanism |
| --- | --- | --- |
| Light↔dark for content | **Yes, instant** | `WebView::notify_theme_change` (verified; fires `matchMedia` change events) |
| Inject/replace a content user stylesheet | **No — needs reload** | `UserContentManager` doc: "take effect only after the page is reloaded" |
| Per-site CSS toggle | **No — needs reload** | same |

**Design consequence:** when the user toggles force-dark or a per-site theme *on the
current page*, swerve must trigger a `webview.reload()` to make the injected sheet take
effect (or inject the same CSS via `evaluate_javascript` as a one-shot `<style>` element
for an instant preview, then persist it through `UserContentManager` for future loads).
The pragmatic pattern:

> **Preview-then-persist:** for an instant effect, `evaluate_javascript` a
> `document.head.appendChild(<style>…)` into the content document (hot, no reload).
> Simultaneously register the CSS with the tab's `UserContentManager` so it survives
> navigation. New loads get it natively; the current load got the JS-injected preview.

This is a real, shippable technique and it sidesteps the reload-only limitation for the
common "I just toggled dark mode" case. The only caveat is CSP: a strict
`style-src` CSP can block a JS-injected `<style>`. `UserContentManager` user stylesheets
are *not* subject to page CSP (they're embedder-origin), so persistence always works even
when the instant preview is CSP-blocked — in that case fall back to a reload.

### 4.3 Force-dark content stylesheet (the GX-style "dark everything")

`notify_theme_change(Theme::Dark)` only sets the media feature; sites that don't honor
`prefers-color-scheme` stay light. For true force-dark we inject a user stylesheet at
`Origin::User`. Because user origin loses to author rules by default, the sheet must use
either `!important` (user-important beats author-important per CSS cascade — verified by
the Servo test where a non-`!important` user rule lost to author) **or** an `@layer`
strategy. A conservative starting sheet:

```css
/* swerve force-dark (user origin). Coarse but safe; refine per-site later. */
:root { color-scheme: dark !important; }
html, body { background-color: #16181c !important; color: #d7dade !important; }
img, video, picture, [style*="background-image"] {
  filter: brightness(0.9) !important;   /* `filter` IS wired in Servo */
}
```

A more advanced force-dark (Chrome's auto-dark style: invert + hue-rotate the whole page,
then re-invert media) is expressible with `filter: invert(1) hue-rotate(180deg)` on `html`
and `invert(1) hue-rotate(180deg)` on `img/video` — and `filter` is confirmed wired to
WebRender, so this renders. Quality will be below Chrome's content-aware dark mode (which
is engine-internal), but it is a legitimate v1.

### 4.4 Per-site themes — embedder-driven targeting (no `@-moz-document`)

Since `@-moz-document` is disabled in Servo, swerve cannot ship a single user stylesheet
that self-targets by domain. Instead the **embedder** decides what to attach:

- Each tab's `UserContentManager` is composed by the engine from: global force-dark (if
  on) + the matching per-site sheet for that tab's current origin.
- On navigation (`notify_url_changed`, already a delegate we implement), recompute the set
  of sheets for the new origin and — because adds need a reload — apply via the
  preview-then-persist pattern (§4.2), or simply rebuild the tab's `UserContentManager`
  and let the next load pick it up.
- Per-site theme storage key = registrable domain (eTLD+1). We own the URL→sheet map; the
  CSS files themselves are plain CSS with **no** `@-moz-document` wrapper.

**Importing userstyles.org / Stylus styles:** we can offer a one-way importer that parses
the `==UserStyle==` metadata block and the `@-moz-document` conditions, *strips* the
`@-moz-document` wrapper, and registers the inner CSS against the extracted domains in our
own URL→sheet map. This gives compatibility with the existing ecosystem without needing
Servo to parse `@-moz-document`. (Stylus `@var`/preprocessor support is a stretch goal;
v1 imports plain CSS userstyles only.)

---

## 5. Fonts

| Font target | Mechanism | Hot? | Constraint |
| --- | --- | --- | --- |
| **Chrome UI font (system)** | `--sw-font-ui` token | Yes | none — system font stacks always work |
| **Chrome UI font (bundled custom)** | `@font-face` in chrome CSS | Yes, after enabling | requires `dom_fontface_enabled = true` (OFF by default) **and** an asset URL the chrome can load (§6) |
| **Variable fonts in chrome** | `font-variation-settings` | after enabling | requires `layout_variable_fonts_enabled = true` (OFF by default); landed Sept 2025, weight/stretch applied Nov 2025 |
| **Content default font** | `fonts.default` / `fonts.serif` / `fonts.sans_serif` / `fonts.monospace` + sizes | **Needs reload** | These are **global** `Preferences`, not per-WebView. Set via `Servo::set_preference("fonts.default", …)`; affects all tabs; new loads only |
| **Content per-site font** | a per-site user stylesheet setting `font-family` | Needs reload | rides the §4.4 per-site mechanism |

**Recommendation:**
- Enable `dom_fontface_enabled` + `layout_variable_fonts_enabled` in
  `ServoBuilder::preferences` so the chrome can ship a bundled UI font (a key part of a
  distinctive look). Verify the chrome still renders after enabling (these are gated for
  stability reasons; treat enabling as a reviewed change with a visual check).
- Treat **content** font customization as a coarse, global, reload-required setting in v1
  ("Default page font"). Do not promise live per-tab content font switching — the API
  doesn't support it cleanly.
- Bundle 2–4 curated UI fonts (one geometric sans, one humanist sans, one mono) rather
  than allowing arbitrary font files in v1 (font files are an attack surface; see §8).

---

## 6. Asset serving (wallpapers, fonts, icons, sounds)

The chrome is a `file://` document. Referencing theme assets as `file://` paths is fragile
(sandboxing, sync, marketplace installs from arbitrary dirs). Use Servo's
**`ProtocolRegistry`** (`ServoBuilder::protocol_registry`, verified `register()` in
`net/protocols/mod.rs`) to register a custom scheme, e.g. `swerve-asset://`:

- `swerve-asset://theme/<theme-id>/<path>` → resolved by a Rust `ProtocolHandler` to a
  file inside the installed theme's sandboxed directory (and *only* there — path-traversal
  checked).
- Benefits: stable URLs in CSS (`--sw-wallpaper: url(swerve-asset://theme/aurora/bg.webp)`),
  no leaking absolute paths, a single choke point to enforce theme-dir confinement and
  asset-type/size limits, and it works identically for chrome and (if ever needed) content.
- The handler caps asset size and validates MIME by extension to keep the perf budget
  (§7) and limit attack surface (§8).

This is a small, well-bounded amount of Servo-facing code (one trait impl), and it's the
right foundation for the marketplace.

---

## 7. Performance budget (the actual differentiator)

GX's reputation is the cautionary tale: 650 MB–1.2 GB RAM, 80–100% CPU spikes
([Opera forums][gx-cpu]). swerve's promise only holds if the theming layer is provably
cheap. Concrete budget and the reasons it's achievable here:

| Metric | Budget | Why achievable / how enforced |
| --- | --- | --- |
| Chrome idle CPU | **~0% when not interacting** | The chrome is a static HTML doc; with `--sw-motion:0` or no running animation it should not repaint. swerve already redraws only on `notify_new_frame_ready` / input. **Rule: no infinite CSS animation in a default theme.** Marketplace themes with looping animation get a perf flag. |
| Theme apply (token change) | **< 16 ms** (one frame) | It's a `setProperty` storm on one `:root` → single style recalc. Measure via a timestamp round-trip on `swerve:theme`. |
| Theme/mod package install | **< 1 s** for a typical package | Unzip + validate + register assets; cap package size (§9). |
| Per-theme RAM overhead | **< 5 MB** decoded | Cap wallpaper to one decoded image; cap dimensions; prefer WebP/AVIF; lazy-decode. Enforced in the `swerve-asset` handler. |
| Animation cost | **GPU-composited only** | Animate `transform`/`opacity` (WebRender-composited) — **not** `width`/`top`/`background` (layout/paint). `filter` blur is GPU but expensive; cap blur radius in default themes. |
| Sound latency | **< 50 ms** click→sound | Preload + decode `<audio>`/WebAudio buffers at theme load, not on event. If Servo audio latency is too high (measure — gstreamer is off by default; enabling it adds deps), ship sounds OFF by default and document the cost. |
| Mod execution | **time-sliced; no busy loops** | Mod API runs in the chrome JS context; a watchdog disables a mod whose handlers exceed a frame-time budget repeatedly (§8). |

**The hard rule that beats GX:** *a theme cannot run anything continuously.* GX's cost is
partly the always-animating, always-shimmering UI. swerve's default themes are static;
motion happens only on interaction and only on composited properties. We expose this as a
**theme perf score** (computed at install: counts infinite animations, blur usage,
wallpaper size, mod handler count) shown in the marketplace and settings.

**Measurement plan:** add an internal `swerve:perf` event the chrome can emit with
`performance.now()` deltas around theme apply, and an engine-side frame-time/RSS sampler.
Wire these into the headless Xvfb harness already used for milestone verification so perf
regressions are caught in CI, not by users.

---

## 8. Chrome-mod API (safe extensibility)

Mods are more than themes: they add UI (a button, a sidebar panel) or behavior (a new
gesture). The chrome is JS we control, so mods are **scoped JS + CSS injected into the
chrome document**, not arbitrary native code. Security model:

- **No raw DOM-of-trust:** a mod does *not* get free reign over `document`. It gets a
  capability object `swerve.mods.api` with a curated, versioned surface:
  ```js
  // exposed to a mod's sandboxed entry script
  const api = {
    version: 1,
    ui: { addToolbarButton({id,label,icon,onClick}), addSidebarPanel({...}), removeOwn() },
    tabs: { list(), onChanged(cb), active() },           // read-only mirror of the tab model
    nav:  { navigate(url), back(), forward(), reload() },// already-allowed engine verbs
    theme:{ getToken(name), setToken(name,val) },        // scoped to mod-owned tokens
    storage: { get(k), set(k,v) },                       // namespaced, quota'd, synced via Lyku
    events: { on(name,cb) },                             // allowlisted engine events only
  };
  ```
- **Capabilities are declared in the manifest and granted at install** (like a permission
  prompt). A mod that didn't request `nav` cannot navigate. The engine enforces this by
  only wiring the requested verbs into that mod's `api`.
- **Isolation:** each mod's script runs in its own scope (an IIFE/module with no access to
  other mods' state and no direct access to the chrome's internal functions). It cannot
  reach the content webviews directly — all engine actions go through `api`, which maps to
  existing `swerve:` verbs the engine already validates. This reuses the trust boundary
  swerve already has (the `swerve:` scheme is the only chrome→engine path).
- **No content access by default.** A mod cannot read page content; that would require an
  explicit, separately-prompted `content-read` capability backed by a per-site
  `UserScript` (and even then, page-world, not privileged).
- **Watchdog:** the engine measures time spent in mod callbacks (the chrome can post
  `swerve:mod-timing`); a mod that repeatedly blows the frame budget is auto-disabled with
  a user notification. This is how we keep the GX perf failure mode from creeping in via
  third-party mods.
- **CSS mods** are the common case and need no JS capability at all — they're just
  additional rules in the managed `<style>` (§4.1), constrained to chrome selectors.

This is a deliberately *small* API in v1 (toolbar button + sidebar panel + theme tokens +
storage). It's enough for the long tail of "I want a button that does X" without opening a
WebExtensions-sized security surface. WebExtensions-grade content scripting is a separate,
later, much larger project — call it out as out of scope for theming v1.

---

## 9. Package format & manifest

A theme or mod is a signed zip (`.swerve` / `.swervemod`) with a strict layout:

```
my-theme.swerve
├── manifest.json          # required; schema below
├── theme.css              # optional: extra chrome rules (managed <style>)
├── tokens.json            # optional: token overrides (preferred over raw CSS)
├── content/
│   ├── force-dark.css      # optional: user-origin force-dark sheet
│   └── sites/
│       └── github.com.css  # optional: per-site sheet (domain = filename)
├── assets/
│   ├── bg.webp             # wallpaper(s)
│   └── click.ogg           # sounds
├── fonts/
│   └── Inter-var.woff2     # bundled UI fonts (only if @font-face enabled)
└── mod/                    # only in .swervemod
    └── main.js             # sandboxed entry, uses swerve.mods.api
```

### 9.1 Manifest schema (theme)

```json
{
  "schema": 1,
  "id": "aurora",
  "type": "theme",
  "name": "Aurora",
  "version": "1.2.0",
  "author": "nicks",
  "license": "MIT",
  "homepage": "https://lyku.example/themes/aurora",
  "min_swerve": "0.5.0",

  "scheme": "dark",
  "tokens": {
    "--sw-color-accent": "#7c5cff",
    "--sw-color-bg": "#0e1014",
    "--sw-color-surface": "#171a21",
    "--sw-font-ui": "\"Inter\", system-ui, sans-serif",
    "--sw-density": "1",
    "--sw-wallpaper": "url(swerve-asset://theme/aurora/bg.webp)",
    "--sw-wallpaper-opacity": "0.10"
  },
  "fonts": [
    { "family": "Inter", "src": "fonts/Inter-var.woff2", "variable": true }
  ],
  "sounds": {
    "enabled": false,
    "events": { "tab_open": "assets/open.ogg", "click": "assets/click.ogg" }
  },
  "content": {
    "force_dark": "content/force-dark.css",
    "sites": { "github.com": "content/sites/github.com.css" }
  },
  "assets": ["assets/bg.webp"],
  "perf": {
    "infinite_animations": 0,
    "uses_blur": false,
    "wallpaper_bytes": 184320
  }
}
```

### 9.2 Manifest schema (mod — superset)

```json
{
  "schema": 1,
  "id": "vertical-tabs",
  "type": "mod",
  "name": "Vertical Tabs",
  "version": "0.3.1",
  "min_swerve": "0.6.0",
  "entry": "mod/main.js",
  "capabilities": ["ui", "tabs", "nav", "storage"],
  "css": "theme.css"
}
```

### 9.3 Validation rules (enforced at install)

- `schema` must match a supported version; `min_swerve` must be ≤ running version.
- `tokens` keys must be in the known token namespace (`--sw-*`); unknown keys rejected
  (forward-compat: warn, don't apply).
- All `src`/`assets`/`content`/font paths must resolve *inside* the package (no `..`, no
  absolute paths) — the `swerve-asset` handler enforces the same at runtime.
- `capabilities` (mods) must be a subset of the known set; each maps to a prompt at install.
- Total uncompressed size, per-asset size, and image dimensions are capped (perf + DoS).
- The `perf` block is *recomputed* by the installer, not trusted from the manifest; the
  recomputed values drive the marketplace perf score.
- **Signature:** packages are signed; swerve verifies the signature against the
  publisher's key (Lyku-issued for marketplace items; self-signed allowed for sideload
  with a clear "unverified" badge). Mods (which run JS) require a stronger trust signal
  than pure-CSS themes.

**Why JSON tokens are preferred over raw CSS:** a `tokens.json`/manifest `tokens` block is
validated, diffable, sync-friendly, and can't smuggle arbitrary CSS that breaks the
chrome. Raw `theme.css` is allowed for power users but is the *escape hatch*, not the
primary path. Most themes are pure token sets and need no CSS file at all.

---

## 10. Sync integration (Lyku)

Themes/mods and the active theme selection are **user data that should sync**. Lyku is the
sync service (self-hostable later). Integration design:

- **What syncs:**
  1. *Settings* — active theme id, density, motion, font choices, force-dark on/off,
     per-site theme map (domain→theme-id), sounds on/off. Small JSON; syncs eagerly.
  2. *User-authored themes/mods* — the package blob. Larger; content-addressed
     (hash = id of the blob), uploaded once, referenced by settings.
  3. *Marketplace installs* — only the reference (marketplace id + version) syncs, not the
     blob; another device re-downloads from the marketplace. Saves bandwidth and keeps the
     signature chain intact.
- **Conflict model:** settings are last-writer-wins per key with a vector clock /
  timestamp (a theme choice is not mergeable; pick the newest). Per-site map merges per
  key (domain), so two devices customizing different sites both win.
- **Bandwidth/perf:** never sync decoded assets; sync the package zip (already compressed),
  and only for user-authored content. A 200 KB theme is fine; the wallpaper size cap (§9)
  keeps blobs small.
- **Privacy:** the per-site theme map is a list of domains the user has visited+themed —
  treat it as sensitive. End-to-end encrypt the synced settings blob (Lyku stores
  ciphertext; this also fits the "no telemetry" positioning). This is a hard requirement,
  not a nicety: shipping a browser that uploads your browsing-adjacent data in plaintext
  would contradict the entire anti-Chrome pitch.
- **Hook into the existing bridge:** sync is an engine-side concern; on a sync pull that
  changes the active theme, the engine just runs the normal §3.3 apply path
  (`swerve:theme` push). The chrome doesn't know or care that the change came from sync vs.
  the settings UI. This keeps sync fully decoupled from the theming runtime.

---

## 11. Marketplace concept

- **Catalog** = Lyku-hosted index of signed packages with metadata + the recomputed perf
  score (§7) prominently shown. Sort/filter by perf score, type (theme/mod), scheme.
- **Trust tiers:** *Verified* (Lyku-signed, perf-audited, mods code-reviewed) vs.
  *Community* (signed by author, unverified, clear badge). Mods always show their requested
  capabilities before install (like an app-store permission list).
- **Install flow:** download blob → verify signature → validate manifest (§9.3) →
  recompute perf → for mods, show capability prompt → register assets via `swerve-asset` →
  available immediately (theme apply is hot; mod load injects into chrome).
- **Anti-GX positioning:** the marketplace leads with the perf score and a "static by
  default" badge. The product story is explicitly "looks as good as GX, costs a fraction."
- **Bootstrap without a server:** before Lyku marketplace exists, support **sideload** of
  `.swerve`/`.swervemod` files from disk (drag-in or a file picker), with the unverified
  badge. This lets the ecosystem start (and lets us dogfood the format) before any backend
  is built — mirrors how Firefox userChrome/FirefoxCSS-Store communities operate
  ([userchrome.org][uc], [FirefoxCSS-Store][fcs]).

---

## 12. Risks & open questions (honest)

**Risks**

1. **Servo embedding-API churn is the #1 risk** (the Verso lesson, restated for theming).
   The theming layer touches `UserContentManager`, `WebView::notify_theme_change`,
   `ProtocolRegistry`, `Servo::set_preference`, and `evaluate_javascript`. The April 2026
   report explicitly notes `WebView.animating()` and `Servo.site_data_manager()` signature
   changes between 0.0.x→0.2.0 — these APIs *will* move. **Mitigation:** wrap every Servo
   theming touchpoint behind a thin internal `engine::theming` module so a rev bump touches
   one file, not the whole feature.
2. **`backdrop-filter` not rendering** kills the literal frosted-glass GX look. Mitigation:
   build the visual language on `filter`, gradients, layered semi-transparent surfaces, and
   wallpapers — which all render. Revisit if/when Servo wires `backdrop-filter`.
3. **Reload-required content sheets** make per-site theming feel less instant than Chrome
   extensions. Mitigation: the preview-then-persist pattern (§4.2). Residual risk: CSP
   sites where the instant preview is blocked and only the post-reload sheet applies.
4. **Gated features (`@font-face`, grid, variable fonts) are OFF for stability reasons.**
   Enabling them for the chrome could surface Servo bugs. Mitigation: enable behind a flag,
   verify visually in the Xvfb harness, keep flexbox+system-fonts as the always-works
   fallback.
5. **Mod security.** Even a curated JS API is a trust surface. Mitigation: capability
   prompts, signature requirements for mods, watchdog, no content access by default, and a
   deliberately tiny v1 surface.
6. **Sound latency** via Servo audio may be poor (gstreamer off by default; enabling adds
   system deps and weight — contradicts the "no bloat" pitch). Mitigation: sounds OFF by
   default, measured before promising, possibly handled engine-side (native audio) rather
   than through Servo if latency is bad.
7. **Perf budget enforcement is only real if measured in CI.** Without the headless
   frame-time/RSS sampler, "fast" is a claim, not a guarantee — and GX shows how fast a
   customization browser's reputation can sour.

**Open questions**

- Does Servo honor `@layer` *cascade ordering* in layout, or only parse it? (stylo parses
  `@layer`; need a layout-level test before relying on it for force-dark override
  strategy. If not, fall back to `!important` user rules — verified to work.)
- Latency and CPU cost of `notify_theme_change` across many tabs at once (theme switch with
  20 tabs open) — needs measurement.
- Can `Servo::set_preference("fonts.default", …)` change the content font without a full
  restart (only a reload), or does the font system cache at startup? Needs a test.
- What's the real decoded-image memory cost of a full-bleed wallpaper in WebRender at HiDPI,
  to set the §7 wallpaper cap precisely?
- Sideload signature UX: how do we let power users self-sign without a Lyku account while
  keeping the "unverified" warning honest and not annoying?
- Does enabling `dom_fontface_enabled` at this rev actually load `@font-face` from a custom
  `swerve-asset://` URL, or only from `file://`/`http`? Needs a test before promising
  bundled fonts.

---

## 13. Prioritized recommendations

**P0 — foundations (no Servo risk, high leverage):**
1. Refactor `chrome.css` onto the `--sw-*` token catalog (§3.2). Pure CSS; do it first.
2. Add the `swerve:theme` apply path (engine push → `setProperty` on `:root` + managed
   `<style>`). Reuses the existing bridge. This *is* chrome hot-reload (§4.1).
3. Add a disk watcher in the engine that re-pushes `swerve:theme` on theme-file change —
   the author DX loop.
4. Add the `swerve:perf` round-trip + an engine RSS/frame sampler wired into the Xvfb
   harness (§7). Measure before claiming "fast."

**P1 — content theming (the visible differentiator):**
5. Wire `WebView::notify_theme_change` to a per-tab light/dark toggle and a global "follow
   chrome scheme" setting (hot path, low risk — Servo-tested).
6. Implement force-dark via an injected `UserContentManager` user stylesheet +
   preview-then-persist (§4.3, §4.2).
7. Implement per-site sheets with embedder-side URL→sheet targeting (§4.4).
8. Add the `swerve-asset://` protocol handler for wallpapers (§6).

**P2 — packaging, fonts, sync:**
9. Define + validate the `.swerve` manifest/format (§9); support sideload install.
10. Enable + verify `dom_fontface_enabled` + variable fonts; bundle 2–4 UI fonts (§5).
11. Wire theme settings + user packages into Lyku sync with E2E-encrypted settings (§10).

**P3 — ecosystem & extensibility:**
12. The chrome-mod API v1 (toolbar button + sidebar panel + tokens + storage) with
    capability prompts + watchdog (§8).
13. Lyku marketplace with perf scores + trust tiers (§11).
14. Stylus/userstyles.org one-way importer (strip `@-moz-document`, map to per-site) (§4.4).

**Cross-cutting (do alongside everything):**
15. Put every Servo theming touchpoint behind an `engine::theming` wrapper module so the
    inevitable Servo rev bumps cost one file, not the feature (Risk #1).

---

[gx-cpu]: https://forums.opera.com/topic/48016/the-high-cpu-ram-etc-usage-topic-opera-gx
[gx-mem]: https://techysnoop.com/stop-opera-gx-from-using-so-much-memory/
[uc]: https://www.userchrome.org/
[fcs]: https://firefoxcss-store.github.io/
