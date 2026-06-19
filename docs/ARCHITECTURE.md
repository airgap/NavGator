# NavGator architecture

NavGator is a web browser with a **native (Rust / [egui](https://github.com/emilk/egui))
chrome** and **[Servo](https://servo.org) as the page renderer**. The engine is also meant
to be reusable by other apps (Tauri-style) later, so it is quarantined behind a thin facade
crate.

The browser UI — toolbar, tab strip, address bar, menus, settings, dialogs, pickers — is
drawn **directly with egui** and composited over the page. Servo renders *only* web content,
into an offscreen GL texture; each frame egui blits that texture onto its background layer
and paints the chrome on top. This is how **servoshell** (Servo's own reference shell) is
built.

This document records *why* the design is shaped the way it is. Read it before changing the
rendering/compositing code, the engine boundary, or the internal-page scheme.

The whole desktop app is one file — `crates/navgator/src/main.rs` (~2370 lines). The
type names below (`AppState`, `Tab`, `Dialog`, `WebViewDelegate`, `load_web_resource`, …)
are real and live there; the engine types come through `crates/navgator-engine/src/lib.rs`.

---

## 1. The big picture

```
          ┌───────────────────────────────────────────────────────┐
          │  OS window  (borderless winit Window)                  │
          │                                                       │
          │   egui chrome  ── TopBottomPanels + Areas + Windows    │  ← native, on top
          │   ┌─────────────────────────────────────────────────┐ │
          │   │ ◀ ▶ ↻   [ address / omnibox ]        ☰ — ▢ ✕     │ │  toolbar panel
          │   │ [tab][tab][+]                                    │ │  tab-strip panel
          │   │ [bookmark][bookmark]                             │ │  bookmarks panel (if any)
          │   ├─────────────────────────────────────────────────┤ │
          │   │                                                 │ │
          │   │   active page  ← Servo OffscreenRenderingContext │ │  ← page texture,
          │   │   (blitted onto egui's background layer)         │ │     egui background layer
          │   │                                                 │ │
          │   └─────────────────────────────────────────────────┘ │
          └───────────────────────────────────────────────────────┘
```

One `Servo` instance drives every tab. Each tab is a `WebView` that renders into a **shared**
`OffscreenRenderingContext` (an FBO/texture); only the active tab is `show()`n and painted.
The chrome is not a webview — it is egui widgets drawn each frame.

The single struct `AppState` owns the world (`servo`, the two rendering contexts, the
`EguiGlow` instance, the `Vec<Tab>`, settings, the profile, dialogs, the window) **and**
implements Servo's `WebViewDelegate`. A `Weak<AppState>` self-reference lets `&self` delegate
callbacks (which Servo hands `&self`) build new tab webviews that need the owning
`Rc<AppState>` as their delegate.

---

## 2. Why native chrome (the M6 pivot)

Earlier milestones rendered the chrome as a **second Servo WebView of HTML/CSS/JS**, bridged
to the engine over a `navgator:` URL string scheme. That model was replaced by the native
egui chrome. The reasons, in priority order, are the product pitch:

- **Security — a clean privilege boundary.** Privileged actions (open a tab, read history,
  pick a file, grant a permission) are now **direct Rust calls**, not URL messages parsed
  out of a webview. The UI never runs as web content, so there is no privileged document for
  a page to try to reach.
- **Performance / leanness.** No second engine document, no second layout/script pipeline,
  no two-webview compositor handshake. One Servo document tree, for the page.
- **Overlays become trivial.** Context menus, dialogs, pickers, the find bar, autocomplete —
  all are ordinary egui `Area`/`Window`s drawn on top, with **no engine fork patch** and no
  fragile chrome↔content IPC. In the old model these were painful.

The trade is that the chrome is now Rust, not hot-reloadable HTML — an acceptable cost for
the boundary it buys.

### The Verso lesson (why the engine coupling is conservative)

[Verso](https://github.com/versotile-org/verso) was a Servo-based browser doing almost
exactly what NavGator wants. It was archived in **Oct 2025**: it could not keep its embedding
layer in sync with Servo's pace of change on limited funding. Verso embedded Servo the
**low-level** way — ~30 individual component crates (`constellation`, `compositing_traits`,
`script`, …), driving the constellation/compositor itself. Every Servo change risked breaking
that surface.

The lessons baked into NavGator:

1. **Use the high-level `servo` (libservo) umbrella crate**, not the individual components.
   Far smaller surface to track. servoshell's `winit_minimal` is the north star for "what the
   current API looks like."
2. **Pin an exact engine `rev` and bump deliberately.** Never float. A bump is its own
   reviewed change (see §8).
3. **Quarantine all engine contact in one crate** (`navgator-engine`, §4) so engine churn
   touches one place, not the app.

`reference/verso/` is vendored (gitignored) as a *reference implementation* of the hard parts.
Read it; don't depend on it (it's archived and on an old Servo rev).

---

## 3. Crate layout

A three-crate Cargo workspace (`Cargo.toml`, `resolver = "3"`):

```
NavGator/
├── Cargo.toml                    # [workspace] + engine [patch] tables (stylo / webrender forks)
├── rust-toolchain.toml           # stable Rust 1.95.0 (matches the pinned engine rev)
├── crates/
│   ├── navgator-protocol/        # servo-free IPC wire types (no engine dep)
│   │   └── src/lib.rs            #   → IpcCommand + its line parser
│   ├── navgator-engine/          # THE ONLY crate that depends on the Servo fork
│   │   └── src/lib.rs            #   → re-export facade for every engine type the app uses
│   └── navgator/                 # the browser binary
│       └── src/
│           ├── main.rs           #   winit app, egui chrome, compositor, WebViewDelegate, IPC
│           └── content/          #   gator:// internal pages (welcome.html, home.html, about.html)
└── docs/                         # ARCHITECTURE.md (this file), FORK.md, ROADMAP.md, plan/*
```

The split exists to bound maintenance cost:

| Crate | Depends on `servo`? | Job |
| --- | --- | --- |
| `navgator-protocol` | **No** | Stable, servo-free wire types for the external-control IPC surface (§6.4). Independent of the engine so the protocol survives engine churn. |
| `navgator-engine` | **Yes — the only one** | Quarantines the Servo/`embedder_traits`/`http` dependency. Engine type churn lands here, not in the app. |
| `navgator` | No (via the facade) | The whole desktop browser: window, egui chrome, compositing, input, tabs, delegate, persistence, IPC. Touches the engine **only** through `navgator_engine` re-exports. |

---

## 4. The `navgator-engine` quarantine crate

`crates/navgator-engine/src/lib.rs` is the **single point of contact with the Servo fork**.
It depends on three engine-coupled crates and re-exports exactly what the app uses:

- `servo` — the high-level libservo umbrella (our fork; §8). Re-exports cover the core
  embedding types (`Servo`, `ServoBuilder`, `WebView`, `WebViewBuilder`, `WebViewDelegate`,
  the two rendering contexts, `Preferences`), the input types (`InputEvent`, `KeyboardEvent`,
  `MouseButtonEvent`, `MouseMoveEvent`, `WheelEvent`, `Key`/`NamedKey`/`KeyState`, …), the
  delegate request/control types (`SimpleDialog`, `SelectElement`, `ColorPicker`,
  `FilePicker`, `AuthenticationRequest`, `PermissionRequest`, `EmbedderControl`,
  `CreateNewWebViewRequest`, `NavigationRequest`, …), and the favicon/image types
  (`Image`, `PixelFormat`, `RgbColor`).
- `embedder_traits` (package `servo-embedder-traits`) — for `EventLoopWaker` and `JSValue`,
  which `servo` doesn't itself re-export.
- `http` (1.x, version-matched to the engine) — `HeaderMap` / `StatusCode` / `HeaderValue`,
  used to build the `WebResourceResponse` served to `gator://` loads (§5).

Web-resource interception types (`WebResourceLoad`, `WebResourceResponse`) are re-exported
here too, since the `gator://` scheme is served through them.

**Why this matters:** because `navgator-engine` is the only crate naming `servo` types, a
Servo API change (a renamed type, a changed signature) breaks compilation **in this one
crate**. The app imports everything as `use navgator_engine::{…}` (plus
`navgator_engine::http::{…}`). Today it is a thin re-export facade — the binary still works
with Servo's own types directly. The intended next step is to replace the re-exports with a
servo-free NavGator API surface (defined alongside `navgator-protocol`), so an engine type
change touches only this crate and never the app.

---

## 5. The `gator://` internal-page scheme

NavGator serves its own internal pages — the welcome / new-tab page is `gator://welcome` —
without patching the engine and without registering a net-internal `ProtocolHandler`.

Servo asks the embedder to **intercept every resource load before it resolves the scheme**,
via `WebViewDelegate::load_web_resource`. `AppState::load_web_resource` is the whole
mechanism:

1. If the load's URL scheme isn't `gator`, **return immediately** — dropping the
   `WebResourceLoad` without calling `.intercept()` signals "don't intercept," so normal
   `http(s)`/`file`/`data` loads pass straight through to the engine.
2. For a `gator://` URL, switch on the host: `welcome` / `newtab` / `home` render the
   welcome page; anything else returns a small "no such internal page" HTML stub.
3. Build a `WebResourceResponse` (status `200`, `Content-Type: text/html; charset=utf-8`),
   then `load.intercept(response)`, `send_body_data(body)`, `finish()`.

The welcome page is generated by `render_gator_welcome()`, which loads
`include_str!("content/welcome.html")` (compiled into the binary) and string-substitutes the
live template tokens: `__ACCENT__` (the user's accent color), `__SEARCH_TEMPLATE__` and
`__SEARCH_ENGINE__` (the selected search engine + its `%s` template), and `__BOOKMARKS__`
(the user's bookmarks rendered as quick-link tiles, or an empty-state hint). Because the page
is embedded and templated in Rust, it works everywhere with no filesystem dependency — unlike
a `file://` home page, which a packaged build would have to locate on disk.

**This is THE model for any new internal page** (a settings page, a history page, an error
page): add a host arm in `load_web_resource`, add an `content/*.html` template, render it in
a helper like `render_gator_welcome`. No fork patch, no second webview.

> Two non-`gator` content templates exist under `content/` (`home.html`, `about.html`) and
> can be loaded as `file://` URLs via `file_url()` / `resources_dir()` (which resolves
> bundled assets next to the executable in a packaged build, or the source tree under
> `cargo run`). The default new-tab page is `gator://welcome`, not the `file://` home page.

---

## 6. Rendering, input, and the engine bridge (all in `main.rs`)

### 6.1 Compositing — one offscreen texture under egui

At startup (`App::resumed`) NavGator creates a borderless winit `Window`, a
`WindowRenderingContext` over it, and from that an `OffscreenRenderingContext`
(`window_context.offscreen_context(...)`) for page content. `EguiGlow` is created on the
offscreen context's `glow` GL.

Each redraw runs in two steps:

- **`AppState::update()`** runs an egui frame: it applies the theme, uploads any pending
  favicons to GPU textures, draws the chrome panels (unless the page is fullscreen), draws
  the settings window, dialogs, status bar, find bar, and the omnibox autocomplete. The page
  area is everything **below** the chrome — derived from `toolbar_height` (the measured bottom
  of the chrome panels, in logical px), since at the egui `Context` level `available_rect`
  doesn't reflect panel reservations. If the page rect changed, it resizes the offscreen
  context and every tab's webview. It paints the active tab, then enqueues a background-layer
  `PaintCallback` that blits the page FBO (via `content_context.render_to_parent_callback()`)
  into the content rectangle, flipping to GL bottom-left coordinates.
- **`AppState::paint()`** makes the offscreen context current, prepares the window context,
  has egui paint over the blitted page, and `present()`s the window.

Servo's "new frame ready" (`notify_new_frame_ready`) and every state change request a winit
redraw, so the page and chrome stay in sync.

### 6.2 The window itself

Decorations are off (`with_decorations(false)`); NavGator draws its own min/max/close buttons
in the toolbar and implements window dragging (`drag_window` from empty toolbar space) and
edge resizing itself: `resize_direction_at` hit-tests a `RESIZE_BORDER`-wide band at the
window edges and calls `drag_resize_window` with the right `ResizeDirection` + cursor.

### 6.3 Input routing

All input arrives in `App::window_event`. The flow per event:

1. Handle window-level concerns first (close, redraw, resize → resize the window context,
   scale-factor change, cursor tracking, `ModifiersChanged` → update the `ctrl`/`shift`
   flags).
2. Feed the event to egui (`on_window_event`); note whether egui **consumed** it.
3. Decide whether the event also goes to the page. The page gets mouse/wheel/key events
   **only** when egui didn't consume them, the pointer isn't over the chrome (`cy <
   toolbar_dev`), and no dialog is open. Page-bound pointer coordinates are shifted up by the
   device-px chrome height (`toolbar_dev`) so the page's origin is the top of the content
   rect. Mouse/wheel/key events are translated to Servo `InputEvent`s and sent to the active
   tab via `notify_input_event`.

**Keyboard** is the winit→Servo path (there is no second webview to route to): `Ctrl`-based
shortcuts are intercepted in `window_event`'s `KeyboardInput` arm **before** forwarding —
Ctrl+T (new tab) / Ctrl+Shift+T (reopen closed) / Ctrl+W (close) / Ctrl+L (focus omnibox) /
Ctrl+R (reload) / Ctrl+D (bookmark) / Ctrl+F (find) / Ctrl +/-/0 (zoom) / Ctrl+1..9 (select
tab) / Ctrl+Tab / Ctrl+Shift+Tab (cycle). `Esc` closes the find bar, then dialogs, then page
fullscreen. Anything else is mapped by `winit_key_to_servo` (a minimal printable +
editing/nav key map) into a Servo `KeyboardEvent::from_state_and_key` and forwarded to the
page. IME/composition is not yet implemented.

Find-in-page (Ctrl+F) has no native engine API, so it is implemented in JS injected via
`evaluate_javascript` (`FIND_JS` wraps matches in `<span data-ngf>`, scrolls to the first,
returns a count; `find_step` / `find_close` navigate and clean up).

### 6.4 External control (IPC) — `navgator-protocol`

An opt-in Unix-socket control surface lets another process drive the engine. Setting
`NAVGATOR_IPC=/path` binds a socket on a background thread (`start_ipc`). Each line is parsed
by `IpcCommand::parse` (`navgator-protocol`: `navigate <url>`, `new-tab`, `reload`, `back`,
`forward`, `select-tab <i>`, `close-tab <i>`) and posted to the winit loop as
`WakeUp::Ipc(cmd)` (winit's `EventLoopProxy` is `Send`). The UI thread runs it
(`handle_ipc`) and writes events back to connected clients
(`Arc<Mutex<Vec<UnixStream>>>`): `url <tab> <url>` and `title <tab> <title>`. This is a
*control plane* for a standalone NavGator window; rendering the engine into a host app's own
surface is future work where the sync-with-Servo cost concentrates.

---

## 7. The `WebViewDelegate` wiring

`AppState` implements `WebViewDelegate`. This is the entire embedder surface — every callback
the engine makes into NavGator. Per-tab callbacks find their tab by matching the delegate's
`WebView` against the tab list (`tab_index`, relying on `WebView: PartialEq`).

**Page/tab state → chrome:**

- `notify_new_frame_ready` → request a redraw.
- `notify_url_changed` → update the tab's URL, mirror it into the omnibox (unless the user is
  mid-edit, tracked by `location_dirty`), record a history visit, emit the IPC `url` event.
- `notify_page_title_changed` → update the tab title, record history, emit IPC `title`.
- `notify_history_changed` → recompute the tab's back/forward availability.
- `notify_load_status_changed` → toggle the tab's loading spinner; clear stale status text.
- `notify_status_text_changed` → the hovered-link / load status shown in the bottom-left
  status bar.
- `notify_favicon_changed` → decode `webview.favicon()` into an `egui::ColorImage` (handling
  every `PixelFormat`) and stash it as `favicon_pending`; it's uploaded to a GPU texture
  during the next egui frame (`load_favicons`, which needs the `egui::Context`).

**Internal pages:** `load_web_resource` serves `gator://` (§5).

**Tab lifecycle:** `request_create_new` (a page opened a new webview — e.g. `target=_blank`)
builds the webview from the request, attaches `self` as delegate, and adopts it as a new tab;
`notify_closed` closes the matching tab.

**Native overlays — the `Dialog` enum + `show_embedder_control`:** the engine drives every UI
prompt through `show_embedder_control(EmbedderControl)` / `hide_embedder_control`, plus a
couple of dedicated requests. NavGator turns each into a native egui overlay held in
`dialogs: RefCell<Vec<Dialog>>`, drawn by `draw_one_dialog`. The held engine handle is
consumed when the user resolves the overlay; **dropping it unresolved cancels** (the engine's
default). The variants:

| Engine input | `Dialog` variant | Native overlay |
| --- | --- | --- |
| `EmbedderControl::SimpleDialog` (`alert`/`confirm`/`prompt`) | `Simple` | message + optional text field; OK / Cancel → `confirm()` / `dismiss()` |
| `EmbedderControl::SelectElement` | `Select` | flattened `<select>` options (optgroups as headers) → `select(...)` + `submit()` |
| `EmbedderControl::ColorPicker` | `Color` | hex entry → `select(rgb)` + `submit()` |
| `EmbedderControl::FilePicker` | `File` | in-egui `egui-file-dialog` (no native GTK/portal dep), honoring filter patterns and multi-select |
| `request_authentication` (`AuthenticationRequest`) | `Auth` | username / password → `authenticate(user, pass)` |
| `request_permission` (`PermissionRequest`) | `Permission` | Allow / Deny → `allow()` / `deny()` |
| right-click over the page (embedder-synthesized) | `ContextMenu` | egui popup: Back / Forward / Reload |

`hide_embedder_control` (page withdrew a control, e.g. navigated away mid-dialog) drops the
pending engine-backed overlays. `notify_fullscreen_state_changed` toggles `fullscreen` (which
hides the chrome). `request_navigation` currently allows every navigation. IME
(`EmbedderControl::IME`, in effect) is not yet handled.

**Web-feature profile:** at build time `ServoBuilder` is given `navgator_preferences()`, which
turns on a curated set of high-value web-platform APIs that Servo ships disabled by default
(IntersectionObserver, Web Animations, async clipboard, IndexedDB, WebGL2, OffscreenCanvas,
the HTML Sanitizer, Permissions/Notifications/Geolocation, …). Servo's posture is
"everything off"; NavGator's value-add is a distinct, curated default. The permission-gated
APIs are *exposed* here but actually granted through the `request_permission` prompt above.

---

## 8. The Servo fork (engine source)

NavGator builds on **[`airgap/swervo`](https://github.com/airgap/swervo)** — a **maintained
fork** of `servo/servo`. The decision and discipline live in [`FORK.md`](FORK.md) and
[`ROADMAP.md`](ROADMAP.md) §R2: we own the engine and implement web-platform features
ourselves, while still merging *from* upstream on a cadence (a hard fork that stops tracking
upstream rots).

The entire engine surface is forked, not just the umbrella crate:

- `servo` + `embedder_traits` (package `servo-embedder-traits`) are git deps in
  `crates/navgator-engine/Cargo.toml`, pinned to a swervo commit. The pin currently carries
  the **first fork patch** (`patches/navgator-ua`: brands the default User-Agent with a
  `NavGator/0.1.0` token), which proves the patch → pin → build → ship pipeline end to end.
- `stylo` (the CSS engine, 8 crates) and `webrender` (the GPU renderer) are redirected to
  `airgap/*` forks via **workspace-root `[patch]` tables** in the top-level `Cargo.toml`.
  Cargo honors top-level patches across the whole (swervo-transitive) graph, so neither
  requires editing the swervo fork. They are upstream-identical until fork patches land.

`media-gstreamer` is enabled on the `servo` dep so `<video>`/`<audio>` decode via GStreamer.
`baked-in-resources` (default) is kept on — Servo's constellation panics at startup with "No
resource reader registered" otherwise.

**Bumping the engine** is its own reviewed change: pin a new swervo commit in
`navgator-engine/Cargo.toml`, keep `rust-toolchain.toml` in lockstep with Servo's, re-check
`winit_minimal` for API drift, and rebuild from a clean lock. The LLVM-consistency build
notes (a single LLVM on `PATH` + matching `LIBCLANG_PATH` for `mozjs_sys`/`mozangle`) are in
the README.

---

## 9. Persistence

All user data lives under `$XDG_CONFIG_HOME/navgator/` (falling back to `$HOME/.config/...`),
resolved by `settings_path()` / `config_file()`:

- **Settings** — `settings.conf`, a tiny `key=value` file (`search`, `accent`, `dark`).
  Loaded by `load_settings()` into `struct Settings`, written by `save_settings()` whenever
  the in-app Settings window changes a value. The accent + dark/light choice drives the egui
  chrome theme (`build_visuals`) **and** the templated `gator://welcome` page.
- **Profile** — `struct Profile { history, bookmarks }`, persisted as **TSV**:
  - `history.tsv` — `url \t title \t visits` per line. `record_visit()` dedupes by URL and
    increments a visit count (frecency, for omnibox autocomplete ranking via `suggestions`),
    skips `about:`/`data:`/`file:` URLs, caps at 2000 entries, and calls `save_history()`.
  - `bookmarks.tsv` — `url \t title` per line. `toggle_bookmark_active()` (Ctrl+D) adds or
    removes the active page and calls `save_bookmarks()`. Bookmarks render in the chrome's
    bookmarks bar and as `gator://welcome` quick-link tiles.

  TSV cells are sanitized with `tsv_field()` (tabs/newlines → spaces) so the separators stay
  unambiguous.

---

## 10. Mobile / Android — deliberately deferred, not built

**There is no Android port in the tree today** — no `cdylib`/`lib` target, no `android_main`
entry point, no `cargo-apk` packaging, and no `docs/ANDROID.md`. This is a *deliberate*
strategic deferral, documented in [`ROADMAP.md` §2a](ROADMAP.md), not an omission:

- **Architecture mismatch, not just scope.** NavGator's native-egui-over-`OffscreenRenderingContext`
  + winit **desktop** compositing path is **not** the Android EGL embedding path
  (`ports/servoshell/egl/android`, the "Kumo" Android shell). A mobile NavGator is closer to a
  **second front-end** than a recompile: touch input routing, on-screen keyboard / IME
  (already engine-weak — see §6.3), Android lifecycle, EGL/GPU surface management, and APK
  packaging. It is a multi-person-quarter effort, layered on an engine whose own Android port
  is still young.
- **A named trigger, not a silent drop.** Mobile is a **post-1.0, separately-funded program**,
  gated on Servo's Android shell reaching daily-driver stability *and* NavGator securing the
  headcount. The desktop platform targets (Linux validated; macOS/Windows in CI per
  [`FORK.md`](FORK.md)) are first-class; mobile is not, yet.

When that program starts, the engine-quarantine crate (§4) and the servo-free protocol crate
(§3) are the seams it would build a second front-end behind — the desktop `main.rs` would not
be recompiled for Android, it would be replaced by an EGL/Android shell talking to the same
engine boundary.

---

## 11. Where to look

| You want to change… | Start at |
| --- | --- |
| the toolbar / tabs / bookmarks bar / settings | `draw_chrome`, `draw_settings` in `main.rs` |
| a dialog / picker / context menu | the `Dialog` enum + `draw_one_dialog` + `show_embedder_control` |
| add an internal page | `load_web_resource` + a `content/*.html` template + a `render_*` helper (§5) |
| compositing / the page-under-chrome blit | `AppState::update` / `paint` (§6.1) |
| keyboard shortcuts / input routing | `App::window_event` → `KeyboardInput` arm (§6.3) |
| use a new Servo/`http` type | re-export it in `crates/navgator-engine/src/lib.rs` first (§4) |
| settings / history / bookmarks storage | `Settings`, `Profile`, `save_*` / `load_*` (§9) |
| the external-control protocol | `crates/navgator-protocol/src/lib.rs` + `start_ipc` / `handle_ipc` (§6.4) |
| the engine version / a fork patch | `crates/navgator-engine/Cargo.toml` + [`FORK.md`](FORK.md) (§8) |
