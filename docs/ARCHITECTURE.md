# swerve architecture

swerve is a web browser whose **chrome (its own UI) is HTML rendered by Servo**,
with web pages rendered by Servo alongside it — and the engine is meant to also be
reusable by other apps (Tauri-style) later.

This document records *why* the design is shaped the way it is, and the order we
build it in. Read it before changing the rendering/compositing code.

---

## The Verso lesson (why this is shaped conservatively)

[Verso](https://github.com/versotile-org/verso) was a Servo-based browser doing
almost exactly what swerve wants. It was archived **Oct 8, 2025** because it could
not keep its embedding layer in sync with Servo's pace of change given limited
funding/manpower. Verso embedded Servo the **low-level** way: it depended on ~30
individual Servo component crates (`constellation`, `compositing_traits`, `script`,
…) and drove the constellation/compositor itself (its `compositor.rs` is ~90 KB).
Every Servo change risked breaking that surface.

Two takeaways baked into swerve:

1. **Use the high-level `libservo` (`servo`) umbrella crate**, not the individual
   components. Far smaller surface to track. The maintained `winit_minimal` example
   is our north star for "what the current API looks like."
2. **Pin an exact Servo `rev` and bump deliberately.** Never float. The pinned rev,
   `rust-toolchain.toml`, and (if needed) a copied `Cargo.lock` move together as one
   unit. Treat a Servo bump as its own reviewed change.

`reference/verso/` is vendored (gitignored) as a *reference implementation* of the
hard parts — multi-webview layout, the chrome↔content IPC, prompts/menus. Read it;
don't depend on it (it's archived and on an old Servo rev).

---

## Why not an `<iframe>` for content

The original instinct — chrome HTML with the page in an `<iframe>` — does not work
for a general browser: a large fraction of the web sends `X-Frame-Options: DENY` or
CSP `frame-ancestors`, and simply refuses to render in a frame (Google, banks, most
login pages). Iframes also make navigation control, isolation, and input routing
awkward. So content is a **separate top-level webview**, not a framed document.

## Why not "just two webviews in one window"

The modern `libservo` `WebView` **fills its `RenderingContext`** — it has
`resize/size/show/hide/paint`, but *no* API to place a webview at a sub-rectangle of
the window. Multiple webviews sharing one context behave like stacked layers (tabs/
overlays), not side-by-side regions. So "toolbar on top, page below" is **not** a
two-`WebViewBuilder` call.

## How the regions actually compose (the servoshell pattern)

`servoshell`'s `Minibrowser` reserves a toolbar height and renders the content
webview into an **`OffscreenRenderingContext`** (an FBO/texture), then composites
that texture into the window below the toolbar. swerve uses the same mechanism, with
**one twist: the chrome is a second Servo webview rendering our HTML**, instead of
egui.

```
            ┌─────────────────────────────────────────────┐
            │  OS window  (WindowRenderingContext)         │
            │  ┌───────────────────────────────────────┐  │
  chrome ──►│  │  chrome webview  → offscreen texture A │  │  composited
  (HTML)    │  └───────────────────────────────────────┘  │  at top
            │  ┌───────────────────────────────────────┐  │
  content ─►│  │  content webview → offscreen texture B │  │  composited
  (page)    │  │                                       │  │  below chrome
            │  └───────────────────────────────────────┘  │
            └─────────────────────────────────────────────┘
```

The embedder owns the layout math (toolbar height, content rect, HiDPI scale),
blits the textures into the `WindowRenderingContext`, and routes input to whichever
webview owns the region under the pointer.

`RenderingContext` (verified at rev `ed1af70`) gives us what this needs:
`read_to_image()`, `create_texture()`/`destroy_texture()`, `size()`, `resize()`,
`make_current()`, `present()`, and gleam/glow GL handles.

---

## Milestones

### M1 — build & event loop green ✅ (verified: builds, runs, renders chrome)
One webview in a winit window, rendering the local HTML chrome by default
(`src/main.rs`). API mirrors `winit_minimal`. **Goal: prove the Servo build,
toolchain, and event loop work end-to-end** before any compositing. This is the step
that sinks projects — de-risked first. Confirmed on Linux via a headless Xvfb +
software-GL run that loaded `src/chrome/index.html` and painted the UI.

### M2 — chrome + content compositing ✅ compositing + mouse input; ⏳ keyboard
- Content webview → `OffscreenRenderingContext`; chrome webview → the window context.
- Each frame: paint both, then `render_to_parent_callback()` scissor-clears + blits
  the content FBO into the region below the chrome (GL bottom-left coords), then present.
- Input: mouse move/button/wheel routed by region (chrome if `y < CHROME_HEIGHT`, else
  content with the point shifted up by the chrome height); clicking a region focuses it.
- **Verified** (headless Xvfb + synthetic `xdotool` input): two webviews / two rendering
  contexts composite in one Servo instance; the `CHROME_HEIGHT_CSS` split aligns; clicking
  the omnibox focuses it and nav buttons hover — i.e. mouse routes to the right region.
- TODO: keyboard (deferred to M3 — needs the winit→keyboard_types mapping in servoshell's
  `keyutils.rs`); verify resize interactively.

### M3 — chrome ↔ engine bridge ✅ keyboard + navigation (verified)
A single-process bridge — **no `ipc-channel`** (Verso needed that only because
versoview is a separate process):
- **Keyboard**: winit key events → Servo `KeyboardEvent` (`winit_key_to_servo` +
  `KeyboardEvent::from_state_and_key`), routed to the focused webview. Minimal key
  map (printable + editing/nav keys); full coverage would adapt servoshell's
  `keyutils.rs`.
- **Chrome → engine**: chrome JS sets `location.href` to a `swerve:` command URL
  (`swerve:nav#<url>`, `swerve:{back,forward,reload}`); the chrome webview's
  `request_navigation` delegate intercepts the `swerve:` scheme, `deny()`s it, and
  drives the content webview (`load`/`reload`/`go_back`/`go_forward`).
- **Engine → chrome**: the content webview's `notify_url_changed` /
  `notify_page_title_changed` / `notify_history_changed` → `WebView::evaluate_javascript`
  dispatching the `swerve:state` event (chrome.js updates the URL bar, tab title,
  back/forward state). chrome.js select-all-on-focus + don't-clobber-while-typing.
- **Verified** (headless Xvfb + `xdotool`): type a URL in the omnibox → Enter →
  content navigates and the address bar + tab title update. Note: synthetic keys
  need `xdotool windowfocus` first (no WM under Xvfb to assign X input focus).
- TODO: dynamic content-rect reporting (retire fixed `CHROME_HEIGHT`); IME/composition;
  a less hacky command channel than `swerve:` navigation; popup/prompt/context-menu hooks.

### M4 — tabs / multi-content
Multiple content webviews (one per tab), one chrome. Show/hide + composite the
active tab's texture.

### M5 — external engine (the Tauri goal)
Factor the engine + a stable IPC surface into a reusable `versoview`-style component
so other apps can depend on it out-of-process — the original Verso/tauri-runtime-verso
goal. **Commit to this only after feeling M1–M4's maintenance cost**, because the
external contract multiplies the sync-with-Servo burden that ended Verso.

---

## Pinned versions (keep in lockstep)

| Thing            | Value                                      |
| ---------------- | ------------------------------------------ |
| servo rev        | `ed1af70e712aa7ae0df4611241f10f6204389b70` |
| Rust toolchain   | `1.95.0` (stable — no nightly)             |
| edition          | 2024                                       |

When bumping the servo rev: update `Cargo.toml` (`servo` + `embedder_traits`),
`rust-toolchain.toml` (match servo's), re-check `winit_minimal` for API drift, and
rebuild from a clean lock.

**Gotchas seen at `ed1af70` (re-verify on every bump):**
- The embedder-traits crate's *package* was renamed to `servo-embedder-traits`
  (lib name still `embedder_traits`) — hence `package = "..."` in `Cargo.toml`.
  Servo renames crates freely; a "no matching package" error means another rename.
- Servo's `[patch.crates-io]` is all commented out here, so external embedders need
  no patch replication. `webrender`/`webrender_api` resolve from crates.io (`0.69`);
  `stylo` is a git dep resolved transitively. If a future rev re-introduces patches,
  they must be copied into swerve's `Cargo.toml` (cargo ignores a dependency's own
  `[patch]`).
- Resolution confirmed: `cargo generate-lockfile` → 848 packages, stable 1.95.
- **An embedder must register a resource reader.** Servo's constellation reads
  bundled resources (UA stylesheets, error pages) and panics at startup with
  "No resource reader registered" otherwise. The easy path is Servo's default
  `baked-in-resources` feature (on by default) — so do **not** pass
  `default-features = false` to the `servo` dep without re-adding it. To ship your
  own resources instead, implement `ResourceReaderMethods` and register it with the
  `servo::submit_resource_reader!(&READER)` macro (inventory-based, at module scope).
- **Build env (this machine, clang 18 + LLVM 21 also present):** `mozjs_sys`/
  `mozangle` need a single consistent LLVM — bare `llvm-objdump` on `PATH` and
  `LIBCLANG_PATH` pointing at the *matching* libclang. See the README.
