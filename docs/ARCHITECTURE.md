# navgator architecture

navgator is a web browser with a **native (Rust / egui) chrome** and **Servo as the page
renderer** — the engine is also meant to be reusable by other apps (Tauri-style) later.

This document records *why* the design is shaped the way it is. Read it before changing the
rendering/compositing code.

> ## M6 pivot — native chrome (current architecture)
>
> The chrome (toolbar, tabs, address bar, menus, dialogs) is drawn with **egui directly over
> the page**, not as a second Servo webview of HTML. Servo renders web content into an
> `OffscreenRenderingContext`; each frame egui blits that texture onto its background layer
> (`render_to_parent_callback`) and draws the chrome on top. This is servoshell's model.
>
> It replaced the original "chrome = a second Servo WebView rendering HTML, bridged over a
> `navgator:` URL scheme" design, which forced a fragile, non-deterministic two-webview
> compositor and made overlays (context menu, dialogs, pickers) painful. Native chrome makes
> overlays trivial egui `Area`/`Window`s (no engine fork patch), is leaner (no second engine
> document), and gives a clean privilege boundary — privileged actions are direct Rust calls,
> not URL messages from a webview. **Security + performance are the product pitch.**
>
> **The sections below — from "Why not an `<iframe>`" through the compositing notes — describe
> the pre-M6 HTML-chrome design and are kept for history.** The Verso lesson and the
> high-level-`libservo` + pinned-rev engine strategy (next section) remain in force.

---

## The Verso lesson (why this is shaped conservatively)

[Verso](https://github.com/versotile-org/verso) was a Servo-based browser doing
almost exactly what navgator wants. It was archived **Oct 8, 2025** because it could
not keep its embedding layer in sync with Servo's pace of change given limited
funding/manpower. Verso embedded Servo the **low-level** way: it depended on ~30
individual Servo component crates (`constellation`, `compositing_traits`, `script`,
…) and drove the constellation/compositor itself (its `compositor.rs` is ~90 KB).
Every Servo change risked breaking that surface.

Two takeaways baked into navgator:

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
that texture into the window below the toolbar. navgator uses the same mechanism, with
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
- Keyboard landed in M3; dynamic content-rect (retiring fixed `CHROME_HEIGHT`) landed
  in M4. Resize: code-complete (the `Resized` handler resizes window + offscreen +
  every tab), and the same content-resize path is exercised by the layout handshake,
  but a true window-resize event can't be triggered headlessly without a WM, so it's
  not yet interactively verified.

### M3 — chrome ↔ engine bridge ✅ keyboard + navigation (verified)
A single-process bridge — **no `ipc-channel`** (Verso needed that only because
versoview is a separate process):
- **Keyboard**: winit key events → Servo `KeyboardEvent` (`winit_key_to_servo` +
  `KeyboardEvent::from_state_and_key`), routed to the focused webview. Minimal key
  map (printable + editing/nav keys); full coverage would adapt servoshell's
  `keyutils.rs`.
- **Chrome → engine**: chrome JS sets `location.href` to a `navgator:` command URL
  (`navgator:nav#<url>`, `navgator:{back,forward,reload}`); the chrome webview's
  `request_navigation` delegate intercepts the `navgator:` scheme, `deny()`s it, and
  drives the content webview (`load`/`reload`/`go_back`/`go_forward`).
- **Engine → chrome**: the content webview's `notify_url_changed` /
  `notify_page_title_changed` / `notify_history_changed` → `WebView::evaluate_javascript`
  dispatching the `navgator:state` event (chrome.js updates the URL bar, tab title,
  back/forward state). chrome.js select-all-on-focus + don't-clobber-while-typing.
- **Verified** (headless Xvfb + `xdotool`): type a URL in the omnibox → Enter →
  content navigates and the address bar + tab title update. Note: synthetic keys
  need `xdotool windowfocus` first (no WM under Xvfb to assign X input focus).
- TODO: dynamic content-rect reporting (retire fixed `CHROME_HEIGHT`); IME/composition;
  a less hacky command channel than `navgator:` navigation; popup/prompt/context-menu hooks.

### M4 — tabs / multi-content ✅ (verified) + dynamic content-rect (M3 polish)
- **Tabs**: content is a `Vec<Tab>` of webviews sharing the one
  `OffscreenRenderingContext`; only the active tab is `show()`n and painted. The
  engine pushes a tab model (`{tabs:[{title}], active, url, canGoBack/Forward}`) to
  the chrome via `navgator:state`; the chrome renders the strip and sends
  `navgator:tab?new|select=i|close=i` back. Per-tab URL/title are attributed by
  matching the delegate's `WebView` against the tab list (`WebView: PartialEq`).
  A `Weak<AppState>` self-ref lets `&self` delegate callbacks build new tab webviews.
- **Dynamic content-rect** (retires fixed `CHROME_HEIGHT`): on load/resize the chrome
  reports its `.viewport` top (CSS px) via `navgator:ready?top=` / `navgator:layout?top=`;
  the engine scales it to device px and derives the content rect, resizing the
  offscreen context + tabs.
- **Verified** (headless Xvfb + `xdotool`): new tab, per-tab navigation (active tab →
  about page, that tab's title updates while the other stays "New tab"), and switching
  tabs swaps the composited content + address bar. Tab strip is rendered from the model.

### M4b — left for later
Drag-reorder tabs, tab overflow/scroll, favicons. (Keyboard tab shortcuts — Ctrl+T
new, Ctrl+W close, Ctrl+Tab next — are done, tracking a `ctrl` modifier flag from
`ModifiersChanged` and consuming those keys before forwarding to the webview.)

### M5 — external engine (the Tauri goal) — ⚙️ v0: IPC control surface (verified)
**Done (v0):** an opt-in IPC control socket (`NAVGATOR_IPC=/path`; Unix socket, text
protocol, no deps) lets an external process drive the engine and receive state:
- commands in: `navigate <url>`, `new-tab`, `reload`, `back`, `forward`,
  `select-tab <i>`, `close-tab <i>`
- events out: `url <tab> <url>`, `title <tab> <title>`

The IPC thread parses each line and posts `WakeUp::Ipc(cmd)` to the winit loop
(winit's `EventLoopProxy` is `Send`); the UI thread runs it and writes events back to
connected clients (`Arc<Mutex<Vec<UnixStream>>>`). **Verified**: drove navigation +
a new tab from `socat` with no window input, and read the emitted events.

**Not done (the big lift):** this is a *control plane* for a standalone navgator
window. The full Verso/`tauri-runtime-verso` model also renders the engine *into the
host app's own window/surface* (host passes a window/surface handle; the engine
composites into it), plus a client crate and a stable, versioned protocol. That
surface-sharing + protocol-stability work is where the sync-with-Servo maintenance
cost concentrates — scope it deliberately. Windows would need a named pipe.

---

## Pinned versions (keep in lockstep)

> **Engine strategy update (per [`ROADMAP.md` §R2](ROADMAP.md), [`FORK.md`](FORK.md)):**
> navgator now builds on our **maintained fork** `github.com/airgap/swervo`, not upstream
> `servo/servo`. We own the engine and implement features in the fork; we merge upstream
> on a cadence but do not file upstream. The "high-level `libservo` + track-upstream"
> stance in *The Verso lesson* above is superseded — though using the umbrella `servo`
> crate and pinning an exact rev still hold. `rev` below is still upstream-identical
> until our first fork patch lands.

| Thing            | Value                                      |
| ---------------- | ------------------------------------------ |
| engine source    | `github.com/airgap/swervo` (fork of `servo/servo`) |
| pinned rev       | `ed1af70e712aa7ae0df4611241f10f6204389b70` |
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
  they must be copied into navgator's `Cargo.toml` (cargo ignores a dependency's own
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
