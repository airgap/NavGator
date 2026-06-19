# navgator — software architecture to scale the prototype

> Dimension: **Software architecture to scale the prototype from today's
> single-binary to a full, industry-standard browser.**
> Date: 2026-06-18. Servo pinned at `ed1af70`. Stable Rust 1.95, edition 2024.
> All "current state" facts below are read from `/raid/navgator/src/`,
> `docs/ARCHITECTURE.md`, `Cargo.toml/Cargo.lock`, and the cached Servo tree at
> `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`.

---

## 0. TL;DR recommendations (prioritized)

1. **Decompose the single binary into a Cargo workspace of ~10 crates _now_,
   while it is cheap.** One `navgator-servo` crate is the *only* crate that may
   `use servo::*` — it is the firebreak against Servo churn (the thing that
   killed Verso). Everything else talks to it through a navgator-owned, stable
   trait/event API that does not leak Servo types.
2. **Keep the engine in-process for v1** (one binary, one Servo, multiple
   webviews — exactly today's model). Do **not** copy versoview's
   separate-process split yet: Servo's own multiprocess+sandbox is *Linux/macOS
   only and off by default* as of 2026, so an out-of-process split buys you
   nothing for security today and costs you a stable wire protocol to maintain.
   Design the internal boundary as a message/command bus so the split is a later
   transport swap, not a rewrite.
3. **Build the chrome as a real app with a tiny reactive layer, not a framework.**
   Servo renders the chrome HTML; Servo's CSS/JS support has real gaps (no
   `text-overflow: ellipsis`, `user-select` is an inert stub — both already
   worked around in `chrome.js`). Ship a ~3-5 KB hand-rolled signals/render
   layer (or Preact-as-a-polyfill-tested dependency), **not** React/Vue/Svelte.
   Define a versioned, typed `navgator:`-message protocol to replace the current
   `location.href = "navgator:..."` hack.
4. **Own the data layer with SQLite (`rusqlite`, bundled) for a "places" DB**
   (history/bookmarks/autofill/permissions). Let Servo own per-profile web
   storage (cookies/localStorage/IndexedDB/HTTP cache) by setting
   `Opts.config_dir` per profile — Servo already persists these and exposes
   `SiteDataManager`/`NetworkManager` to clear them.
5. **Treat a Servo rev bump as a first-class, gated CI event**: it must rebuild,
   pass the headless smoke test + WebDriver UI tests, and re-diff
   `winit_minimal`. The maintenance treadmill is risk #1; make it a tracked,
   automated chore, not a heroic manual port.

---

## 1. Current state (verified facts)

| Fact | Value | Source |
| --- | --- | --- |
| Crates | **1** binary crate (`navgator`) | `Cargo.toml` |
| Rust LOC | `src/main.rs` = **788 lines**; everything in one file | `wc -l` |
| Chrome | **Native egui** (toolbar, tabs, dialogs, menus, settings drawn directly with egui); no HTML/CSS/JS chrome assets | `crates/navgator/src/main.rs` |
| Dependency graph | **855** packages in `Cargo.lock` | `grep -c 'name =' Cargo.lock` |
| Direct deps | `servo`, `embedder_traits`, `winit 0.30`, `euclid`, `dpi`, `url`, `rustls` | `Cargo.toml` |
| Engine embedding | High-level `libservo` umbrella crate, **pinned rev**, default features (incl. `baked-in-resources`) | `Cargo.toml`, `ARCHITECTURE.md` |
| Process model | **Single process, single thread** UI; Servo in-process; one IPC background thread for the optional control socket | `src/main.rs` |
| Engine↔UI | In-process, **direct Rust method calls**. The egui chrome calls `WebView`/`AppState` methods directly (`webview.load(url)`, etc.); there is no `navgator:`-URL command bridge and no `evaluate_javascript` chrome-state push. (`evaluate_javascript` is now used only to run find-in-page JS *inside page content*.) | `main.rs` |
| External control | Opt-in Unix-socket text protocol (`NAVGATOR_IPC`), ~7 verbs in / 2 events out | `main.rs` `start_ipc` |
| Data layer | **None.** No history/bookmarks DB, no profiles, no settings persistence. Servo's own per-site storage uses its default `config_dir` | `main.rs`, Servo `opts.rs` |
| State holder | One `Rc<AppState>` with `RefCell`/`Cell` interior mutability; `Vec<Tab>` of webviews sharing one `OffscreenRenderingContext` | `main.rs` |
| Platform | Linux only in practice (Unix socket, `std::os::unix`); winit/Servo are cross-platform | `main.rs` |
| Tests | Headless Xvfb + software-GL + synthetic `xdotool`; **no in-repo automated tests** | `ARCHITECTURE.md` |

### What Servo (`ed1af70`) already gives us — read this before building anything

`components/servo/lib.rs` re-exports a far richer surface than `main.rs` uses
today. The architecture should lean on these instead of reinventing them:

- **`SiteDataManager`** (`site_data_manager.rs`): enumerate/clear cookies,
  localStorage, IndexedDB, cache per origin (`StorageType` bitflags);
  `cookies_for_url`, `set_cookie_for_url`, `clear_session_cookies`. → the
  "Clear browsing data" + cookie-manager backend.
- **`NetworkManager`** (`network_manager.rs`): `cache_entries()`, `clear_cache()`.
- **`UserContentManager`** (`user_content_manager.rs`): `add_script` /
  `add_stylesheet` (per-`WebView`). → **the engine for both ad/tracker blocking
  *and* user themes/userstyles** — Opera-GX-class customization rides on this.
- **`ServoBuilder`**: `.opts(Opts)`, `.preferences(Preferences)`,
  `.protocol_registry(ProtocolRegistry)`, `.event_loop_waker(...)`,
  `.webxr_registry(...)`. `Servo::set_preference(name, PrefValue)` at runtime.
- **`Opts.config_dir: Option<PathBuf>`** + `Opts.temporary_storage: bool` →
  **per-profile data directories** are a Servo-native concept; you do not build
  cookie/localStorage persistence yourself.
- **`ProtocolHandler`/`ProtocolRegistry`** (`protocol_handler` module) → register
  a real internal-page scheme for settings, history, newtab. (navgator has since
  done this: the `gator://` scheme — e.g. `gator://welcome` — is served from
  embedded resources via `AppState::load_web_resource`/`render_gator_welcome`,
  with no `file://` and no fake `navgator:` command scheme.)
- **The full `WebViewDelegate`** (38+ callbacks) — `main.rs` implements 5. A real
  browser MUST handle: `show_embedder_control` (alert/confirm/prompt, `<select>`,
  color/file pickers, context menus, IME), `request_permission`,
  `request_authentication` (HTTP basic/proxy auth), `request_create_new`
  (`window.open`/`target=_blank` → new tab), `notify_favicon_changed`,
  `notify_load_status_changed`, `show_notification`, `notify_crashed`,
  `load_web_resource` (request interception → adblock), `notify_media_session_event`.
- **WebDriver**: `Servo::execute_webdriver_command(...)` +
  `webdriver_server::start_server(...)` — servoshell wires a full WebDriver
  embedder (`ports/servoshell/webdriver.rs`, 329 LOC). → our UI/integration test
  harness and WPT runner ride on this; we do not invent a test protocol.
- **`Servo::create_memory_report(...)`** → built-in perf/memory telemetry for an
  `about:memory`-style page, no external profiler needed.

**Implication:** a large slice of "full browser" plumbing is _delegate wiring +
a data layer + chrome UI_, not new engine work. The scaling problem is mostly an
**application-architecture** problem, which is good news.

---

## 2. The engine↔UI boundary (the load-bearing decision)

### 2.1 In-process vs. out-of-process

Verso embedded Servo the **low-level** way (~30 component crates, drove the
constellation/compositor itself, `compositor.rs` ~90 KB) **and** ran the chrome
in a **separate process** (versoview) behind an `ipc-channel` protocol. navgator
already rejected the low-level embedding (good). The remaining question is the
process split.

**Recommendation: stay in-process for v1.** Rationale, grounded in the 2026 facts:

- Servo's **own** multiprocess + sandbox is **Linux/macOS only and not enabled by
  default** (`Opts.multiprocess`/`Opts.sandbox` default `false`;
  `run_content_process` exists but the IPC engine API is still maturing).
  Splitting *your chrome* out of process does **not** sandbox web content — that
  is a separate Servo-internal axis. So an out-of-process chrome buys **zero**
  security today while adding a wire protocol you must keep stable across Servo
  bumps. That stable-protocol maintenance is precisely where versoview's cost
  concentrated (your own `ARCHITECTURE.md` M5 notes this).
- In-process means the chrome↔engine calls are plain Rust method calls
  (`webview.load(url)`), not serialized messages — faster to build, impossible to
  version-skew, trivially debuggable.

**But** design the boundary as if it *will* split, so the split is a transport
swap, not a rewrite:

```
   ┌──────────────────────── navgator-app (the process) ────────────────────────┐
   │                                                                           │
   │   chrome WebView (HTML/JS) ──emits──► EngineCommand ──┐                    │
   │            ▲                                          ▼                    │
   │            │                              ┌────────────────────────┐      │
   │   UiEvent  └──────────────────────────────│   EngineBus (trait)    │      │
   │   (state push)                            │  send(EngineCommand)   │      │
   │                                           │  poll() -> EngineEvent │      │
   │                                           └───────────┬────────────┘      │
   │                                                       ▼                    │
   │                                        navgator-engine (owns Servo,          │
   │                                        tabs, OffscreenRenderingContext,    │
   │                                        WebViewDelegate impl)               │
   └───────────────────────────────────────────────────────────────────────────┘

   v1 transport: in-process function calls / mpsc.   Later: ipc-channel + a
   versioned protocol crate, with navgator-engine as a child process.
```

Concretely: define two navgator-owned enums, `EngineCommand` (Navigate, NewTab,
SelectTab, CloseTab, Reload, Back, Forward, SetZoom, FindInPage, ClearData(...),
SetTheme(...), …) and `EngineEvent` (UrlChanged, TitleChanged, FaviconChanged,
LoadStatus, HistoryChanged, TabCreated/Closed, PermissionRequested,
DialogRequested, CrashReport, …). **Neither enum may contain a `servo::` type** —
that is the firewall. Today's three transports (winit window input,
`navgator:`-URL bridge, and the `NAVGATOR_IPC` socket) all collapse into "produce an
`EngineCommand`; consume an `EngineEvent`." The existing `NAVGATOR_IPC` text
protocol and the `IpcCommand` enum in `main.rs` are the **prototype of this bus**
— they just need to be promoted to the canonical internal API, not a side door.

### 2.2 Threading

Servo's `WebView`/`OffscreenRenderingContext`/`Rc<AppState>` are `!Send` and must
stay on the winit main thread (today's design — correct). Keep all engine calls on
that thread; background work (DB writes, sync, network for sync, update checks)
runs on worker threads that communicate via channels and wake the main loop via
the existing `EventLoopProxy<WakeUp>` pattern (already used for IPC). This is the
right concurrency model and should be formalized: **one engine thread, N service
threads, channels between them.**

---

## 3. Cargo workspace / crate decomposition

Move to a workspace now. Target layout:

```
navgator/
├── Cargo.toml                     # [workspace] members + shared [workspace.dependencies]
├── rust-toolchain.toml            # pinned, moves in lockstep with servo rev
├── crates/
│   ├── navgator-servo/              # ★ THE ONLY crate that `use servo::*`
│   │   │                          #   Owns Servo, tabs, OffscreenRenderingContext,
│   │   │                          #   WebViewDelegate. Exposes EngineCommand/EngineEvent.
│   │   └── src/{lib,engine,tab,delegate,compositor,resource_reader}.rs
│   ├── navgator-protocol/           # EngineCommand/EngineEvent enums + (de)serialization.
│   │   │                          #   NO servo dep. Versioned. Shared by engine + chrome bridge
│   │   │                          #   + IPC + future out-of-process client.
│   ├── navgator-app/                # binary: winit event loop, window, wires engine↔chrome↔services.
│   ├── navgator-chrome-bridge/      # Rust side of the chrome protocol (parse navgator: msgs,
│   │   │                          #   serialize state pushes); host of internal navgator:// pages.
│   ├── navgator-data/               # ★ profiles + "places" SQLite DB (history/bookmarks/autofill/
│   │   │                          #   permissions/site-settings) + blob store. rusqlite (bundled).
│   ├── navgator-settings/           # typed settings schema, layered (default→profile→sync), watch.
│   ├── navgator-theme/              # theme model + compiler → CSS vars + userstyles
│   │   │                          #   (drives UserContentManager.add_stylesheet).
│   ├── navgator-sync/               # "Lyku" client: account, E2E-encrypted records, conflict
│   │   │                          #   resolution; pluggable transport (self-host later).
│   ├── navgator-updater/            # update check + verify (signature) + stage/apply.
│   ├── navgator-ipc/                # external control transport (Unix socket / named pipe) over
│   │   │                          #   navgator-protocol. (Today's start_ipc, promoted.)
│   └── navgator-testkit/            # WebDriver client, Xvfb harness wrappers, golden-image helpers.
├── chrome/                        # the chrome app source (HTML/CSS/TS) — see §5
│   └── (built assets embedded via rust-embed or a build.rs into navgator-chrome-bridge)
└── xtask/                         # cargo-xtask: build chrome, bump-servo, package, run-wpt
```

### Crate dependency rules (the firewall)

```
navgator-app ──► navgator-servo ──► [servo, embedder_traits]   ← ONLY here
   │  │  │  └─► navgator-data, navgator-settings, navgator-theme, navgator-sync,
   │  │  │        navgator-updater, navgator-ipc, navgator-chrome-bridge
   │  │  └────► navgator-protocol  (also used by navgator-servo, navgator-ipc, chrome-bridge)
   └─ every non-engine crate depends on navgator-protocol, NEVER on navgator-servo's servo types.
```

| Crate | Depends on `servo`? | Why it exists / scaling payoff |
| --- | --- | --- |
| `navgator-servo` | **Yes (only one)** | Quarantines churn. A Servo bump touches *this crate and its tests*, not the app. ~`main.rs` engine+tab+delegate logic moves here. |
| `navgator-protocol` | No | Stable, versioned vocabulary. Enables in-proc → out-of-proc swap and the external-engine (Tauri) goal without re-plumbing. |
| `navgator-app` | transitively | Thin: window, event loop, service wiring. The "main" today minus engine internals. |
| `navgator-chrome-bridge` | No | Replaces the `navgator:` `location.href` hack with a typed channel; serves internal pages. |
| `navgator-data` | No | History/bookmarks/autofill = embedder's job (Servo doesn't do it). Independent, unit-testable without Servo. |
| `navgator-settings` | No | Layered config; feeds theme + engine prefs. Sync-aware. |
| `navgator-theme` | No | Theme→CSS compiler. Output consumed by chrome (CSS vars) + `UserContentManager` (page userstyles). |
| `navgator-sync` | No | Lyku. Can be developed/tested entirely headless. |
| `navgator-updater` | No | Platform-specific apply, signature verify. |
| `navgator-ipc` | No | External control plane (M5). Already prototyped. |
| `navgator-testkit` | dev-only | WebDriver + Xvfb + golden images. |

**Why this split scales:** the blast radius of a Servo bump is one crate; the
chrome team can iterate on UI without touching engine code; sync/data/theme are
ordinary Rust libraries testable in milliseconds without booting Servo or a GPU.

---

## 4. The data layer (profiles, places DB, storage)

Servo persists **web-platform** storage (cookies, localStorage, IndexedDB, HTTP
cache) under `Opts.config_dir`. The **browser** data (history, bookmarks, open
tabs, autofill, site permissions, settings, themes) is the embedder's
responsibility and does not exist yet.

### 4.1 Profiles

A profile = one directory tree + one Servo `config_dir`:

```
$XDG_DATA_HOME/navgator/profiles/<id>/
├── places.sqlite          # history, bookmarks, autofill, permissions, tab-session
├── places.sqlite-wal      # (WAL mode)
├── settings.json          # navgator-settings (layered over compiled defaults)
├── themes/                # installed/custom themes
├── blobs/                 # favicons, page thumbnails, downloads metadata
└── servo/                 # Opts.config_dir → Servo's cookies/localStorage/IDB/cache
```

Pass `config_dir = profiles/<id>/servo` to `ServoBuilder.opts()`. Use
`temporary_storage = true` for private/incognito windows. Multiple profiles =
multiple Servo instances (heavy) **or** v1: one profile per process, switch =
relaunch. (Chrome runs one process per profile; matching that is fine.)

### 4.2 The "places" DB — SQLite via `rusqlite` (feature `bundled`)

SQLite is the industry standard here (Firefox `places.sqlite`, Chrome's
`History`/`Favicons` are SQLite). Use `rusqlite` with the **bundled** SQLite so
there is no system dependency (consistent with the no-bloat goal; adds ~one C lib
to the build, compiled once). WAL mode for concurrent read while writing.

Proposed schema (initial):

| Table | Purpose |
| --- | --- |
| `places(id, url, title, rev_host, visit_count, last_visit, frecency)` | canonical URLs + ranking (frecency like Firefox) |
| `visits(id, place_id, ts, type, from_visit)` | history timeline, back-referencing for "typed/linked" |
| `bookmarks(id, place_id, parent, position, title, kind)` | folders + items (tree) |
| `inputhistory(place_id, input, use_count)` | omnibox autocomplete training |
| `favicons(id, data BLOB, mime, ts)` + `icon_map(page_url, favicon_id)` | favicon cache |
| `permissions(origin, feature, decision, ts)` | mirrors `request_permission` decisions |
| `site_settings(origin, zoom, theme_override, js_enabled, ...)` | per-site overrides |
| `autofill_*` | form/address/card data (encrypted columns) |
| `session(window, tab_index, url, scroll, ts)` | crash recovery / restore tabs |
| `schema_version` | migrations |

`navgator-data` exposes async, Servo-free APIs: `record_visit`, `query_history`,
`autocomplete(prefix) -> ranked`, `bookmark/unbookmark`, `set_permission`,
`save_session`. Writes happen on a DB worker thread; the omnibox autocomplete
query path must be fast (<10 ms) — index `places.url` and `places.frecency`.

### 4.3 Blob/cache storage

- **Favicons / thumbnails**: small blobs in SQLite (favicons) or
  `blobs/` content-addressed by hash for thumbnails.
- **Downloads**: stream to disk via `WebResourceLoad` interception
  (`load_web_resource` delegate); metadata row in DB.
- **HTTP cache / service-worker / cache-API**: **leave to Servo** (in
  `config_dir`); expose clearing through `NetworkManager::clear_cache()` and
  `SiteDataManager::clear_site_data(...)`.

### 4.4 Settings (`navgator-settings`)

Layered resolution: **compiled defaults → profile `settings.json` → synced
overrides (Lyku) → command-line/env**. Strongly-typed schema (serde). On change,
fan out to: chrome (push `EngineEvent::SettingsChanged`), engine
(`Servo::set_preference` for the subset that maps to Servo `Preferences`/`Opts`),
and theme compiler. Keep navgator settings (UI/theme/sync) distinct from Servo
engine prefs; map only the safe subset onto Servo.

---

## 5. The chrome as a real app (HTML/CSS/JS, no bloat)

> **Stale (pre-pivot):** this section assumed an HTML chrome rendered by a Servo
> `WebView`. navgator has since moved the chrome to **native egui** (see §1), so the
> two constraints below — Servo CSS/JS gaps in the chrome, and a JS-framework
> runtime inside the browser UI — no longer apply to the chrome. The discussion is
> retained only as the rationale for that pivot.

The chrome was originally HTML rendered by a Servo `WebView`. Two hard constraints shaped the
stack:

1. **Servo's web platform support has gaps.** Already worked around in
   `chrome.js`: no `text-overflow: ellipsis` (manual binary-search truncation),
   `user-select` is an inert stub (manual `selectstart` cancel). Expect more
   gaps. → **The chrome must run on what Servo actually supports**, which rules
   out frameworks that assume a complete modern engine or exotic build output.
2. **No bloat / performant.** A React+Vite chrome would pull a large JS runtime
   into the *browser's own UI* — the opposite of the project's thesis, and it
   would stress the very engine you're embedding.

### 5.1 Recommended stack

| Concern | Recommendation | Why |
| --- | --- | --- |
| Language | **TypeScript**, compiled to ES2020 the embedded Servo supports | type-safe protocol messages; no runtime cost |
| Build | **esbuild** (or Vite using esbuild) → single bundled `chrome.js` + `chrome.css` | fast, zero-config, tree-shaken, tiny output |
| Reactivity | **Tiny signals library** (hand-rolled ~3-5 KB, or `@preact/signals-core` + Preact ~10 KB gz) | fine-grained DOM updates for tabs/settings without a VDOM framework; **must be WPT-gated against Servo** |
| State mgmt | **A single typed store** mirroring `EngineEvent`s → signals; actions emit `EngineCommand`s | one source of truth, matches the protocol crate |
| Styling | **Plain CSS with custom properties** (`--navgator-*`) as the theming substrate; no CSS-in-JS, no Tailwind build | themes = swapping CSS-var values; cheap, Servo-friendly |
| Routing | hash/path within the chrome doc for settings/history/newtab "pages" served via `navgator://` protocol handler | avoids `file://` and the fake-scheme hack |

**Decision: do NOT adopt React/Vue/Svelte/SolidStart.** Adopt a signals
primitive + direct DOM. If a component model is wanted, **Preact** is the
ceiling (smallest mainstream VDOM, ~3 KB core) and only after a WPT-style smoke
suite proves it renders correctly in *this* Servo rev. Lit/web-components is a
viable alternative *iff* Servo's custom-elements + Shadow DOM support is
verified at the pinned rev — **test before committing** (treat as an open
question, §10).

### 5.2 Replace the `navgator:` `location.href` bridge with a typed protocol

The original HTML chrome did `window.location.href = "navgator:nav#" + url`, intercepted
in `request_navigation`. (With the native-egui chrome this bridge no longer exists —
`request_navigation` now simply `allow()`s — and the typed-protocol design below is moot
for the chrome; keep it only as reference for the external `NAVGATOR_IPC`/engine-as-a-service
surface.) This was clever but limited (one-shot, string-only,
collides with real navigation, no responses). Replace with:

- **Chrome → engine**: a small injected JS shim `window.navgator.send(cmd)` that
  serializes a `navgator-protocol` `EngineCommand` to JSON and ships it over **one**
  channel. Cleanest mechanism at this rev: register a **`navgator://command`
  ProtocolHandler** (POST-like body via the URL/fetch) **or** keep the intercept
  but send structured JSON in the fragment and add request/response IDs.
  (`ProtocolHandler` is the more durable choice and also gives you `navgator://`
  internal pages.)
- **Engine → chrome**: keep `WebView::evaluate_javascript` dispatching a single
  `navgator:event` `CustomEvent` carrying a JSON `EngineEvent`. Typed on both
  sides via shared TS types generated from `navgator-protocol` (e.g. `ts-rs` or a
  small codegen in `xtask`).

This makes the chrome a normal event-sourced app and keeps the wire format
identical whether the engine is in-proc or (later) out-of-proc.

### 5.3 Chrome surface to build (UI scope for "full browser")

Tab strip (drag-reorder, overflow, favicons, audio/mute, pinned), omnibox
(autocomplete from `navgator-data`, search suggestions, security indicator),
menu/overflow, **settings app** (multi-section, the big one), history & bookmarks
managers, downloads shelf/panel, find-in-page, permission/cookie prompts (driven
by delegate events), context menus, the theming/customization studio (Opera
GX-class), newtab page, error pages, `about:` pages (`navgator://memory`,
`navgator://settings`, etc.). All of this is `EngineEvent`-driven DOM — which is
why the data/protocol crates must land first.

---

## 6. Theming & customization (the Opera-GX-class differentiator)

Two distinct surfaces, both already supported by Servo primitives:

1. **Chrome theming** — `navgator-theme` compiles a theme (colors, radii, density,
   accent, optional background image/video, animation toggles) into a set of
   `--navgator-*` CSS custom properties injected into the chrome document. Switching
   theme = updating variables (no reload). Performance: keep effects (blur,
   animated accents) behind a "performance mode" flag, and prefer CSS over JS
   animation; measure with `Servo::create_memory_report` + frame timing.
2. **Web-content theming / userstyles / extensions-lite** — via
   `UserContentManager::add_stylesheet` / `add_script` per `WebView`. This is the
   same mechanism that powers **content blocking** (inject hiding CSS + blocking
   scripts) and dark-mode-everywhere. A theme can ship a userstyle; an adblock
   list compiles to user stylesheet rules + `load_web_resource` request blocking.

Themes are sync records (Lyku) and shareable files. Keep the theme schema in
`navgator-protocol`/`navgator-theme` so chrome, engine, and sync agree.

---

## 7. Build / release / auto-update pipeline

### 7.1 Build (`xtask`)

A `cargo-xtask` crate drives multi-step builds without makefiles:
`xtask build` (compile chrome TS→bundle, then `cargo build`), `xtask bump-servo
<rev>` (update both `servo` deps + `embedder_traits` package, sync
`rust-toolchain.toml`, regenerate lock, run smoke), `xtask wpt`, `xtask package`.
The chrome bundle is embedded into `navgator-chrome-bridge` via `rust-embed`/`build.rs`
so the shipping binary is self-contained (no `file://` to source files like today).

### 7.2 The Servo-bump gate (risk #1 control)

Make `xtask bump-servo` a **reviewed, CI-gated** operation:

```
bump-servo:  set rev  →  copy Servo's Cargo.lock for that rev (README already
             documents this fallback)  →  cargo build  →  headless smoke
             →  WebDriver UI suite  →  diff winit_minimal for API drift
             →  open PR titled "servo: <old>..<new>" with the API-drift diff.
```

Cadence: **scheduled monthly** (or on Servo release tags), never automatic
merge. Treat each bump as one atomic, revertible change (already the doctrine in
`ARCHITECTURE.md` — formalize it in CI).

### 7.3 Release & auto-update (`navgator-updater`)

- **Channels**: nightly / beta / stable, each a signed manifest.
- **Format**: full-package downloads first (simple, robust); add binary deltas
  later. Sign manifests + artifacts (minisign/ed25519); `navgator-updater` verifies
  before staging. **No telemetry; update check is the only phone-home, and it
  should be IP-minimal and opt-outable** (this is a selling point vs Chrome).
- **Apply**: platform-specific — Linux: AppImage/`.deb`/`.rpm` + (later) a
  self-update helper; macOS: notarized `.app` + Sparkle-style; Windows: MSIX or
  an updater EXE. v1 can ship "download + notify, user installs"; full
  background-apply is a later milestone.
- **CI matrix**: Linux x86_64 first (the only verified platform today), then
  macOS arm64/x86_64, then Windows x86_64. Cache the Servo build aggressively
  (sccache + the pinned-rev lock makes it reproducible); first build is GB-scale
  and minutes-to-tens-of-minutes (per README), so CI runners need fat caches.

---

## 8. Cross-platform

| Platform | Engine (Servo) | navgator gaps to close | Priority |
| --- | --- | --- | --- |
| **Linux x86_64** | works (verified) | none structural; today's target | **v1** |
| **macOS** (arm64/x86_64) | supported by Servo/winit | code paths assume `std::os::unix` socket — fine; platform menu/IME via servoshell's `platform/macos`; LLVM-pin build note | **v1.x** |
| **Windows x86_64** | supported | **IPC must become a named pipe** (today Unix-socket only, noted in `ARCHITECTURE.md`); updater/MSIX; path handling | **v1.x** |
| **Mobile (Android/OHOS/iOS)** | Servo has `egl/` ports (android/ohos) but no iOS | entirely different windowing/lifecycle; far off | **post-v1, separate track** |

Keep platform-specific code in clearly-scoped modules (`navgator-ipc` transport,
`navgator-updater` apply, `navgator-app` window/menu). winit + Servo already abstract
windowing/GL; the porting cost is chrome integration (native menus, IME, dialogs)
and packaging, not the engine.

---

## 9. Testing strategy

Build the test pyramid on Servo's own facilities + the headless harness already
in use.

| Layer | Tooling | Scope | Runs |
| --- | --- | --- | --- |
| **Unit** | `cargo test` per crate | `navgator-data` (SQLite queries, frecency), `navgator-settings` (layering), `navgator-theme` (compile), `navgator-protocol` (round-trip ser/de), omnibox parsing | every commit, fast (no Servo) |
| **Protocol contract** | `cargo test` + generated TS types | EngineCommand/Event JSON matches chrome's TS types (codegen check) | every commit |
| **Engine integration** | `navgator-servo` tests behind a feature, headless | boot Servo, load a page, assert events fire (load/title/url) | PR (slow, GPU/Xvfb) |
| **UI / e2e** | **WebDriver** via `Servo::execute_webdriver_command` + `webdriver_server` (servoshell pattern) + `navgator-testkit` client | type in omnibox→navigate; open/close tabs; settings toggles persist; theme switch | PR / nightly |
| **Headless smoke** | existing **Xvfb + software-GL + `xdotool`** harness, scripted | boots, renders chrome, composites content, basic input | every PR (the Servo-bump gate) |
| **WPT** | Servo's WPT runner (`tests/wpt` exists in tree) against navgator's engine config | regression-track web-platform conformance for our pinned rev + our prefs | nightly |
| **Perf** | frame timing + `Servo::create_memory_report` snapshots; track startup, tab-open, RSS | catch theming/effects regressions; enforce "performant" goal | nightly, trend-tracked |
| **Golden image** | `RenderingContext::read_to_image()` → PNG diff | chrome layout regressions (esp. after Servo bumps) | PR |

Promote the `xdotool` smoke run into a checked-in script under `navgator-testkit`
so it is the canonical CI gate, and replace synthetic `xdotool` keying with
WebDriver where possible (more deterministic; no WM-focus hacks).

---

## 10. Risks & open questions

**Risks (ranked):**

1. **Servo churn (existential — killed Verso).** Mitigation: single
   `navgator-servo` quarantine crate, servo-free `navgator-protocol`, pinned rev,
   gated `bump-servo` CI, monthly cadence. This is the #1 architectural job.
2. **Servo web-platform gaps surfacing in the chrome** (ellipsis/`user-select`
   already; likely Shadow DOM/custom-elements/grid edge cases). Mitigation:
   minimal chrome stack, a WPT-style smoke suite for the *chrome's own* feature
   needs, polyfill-or-avoid policy.
3. **No web-content sandbox today** (Servo multiprocess/sandbox is Linux/macOS,
   off by default). A v1 "industry-standard" browser without sandboxed content is
   a real security gap vs Chrome; must be roadmapped, and is *not* solved by an
   out-of-process chrome.
4. **Build cost / CI scale**: 855-package graph, multi-GB, LLVM-pin fragility.
   Mitigation: sccache, pinned lock, fat caches; gate platform expansion behind
   green Linux.
5. **Multi-profile = multiple Servo instances** is heavy; v1 should pick
   process-per-profile (relaunch to switch) rather than concurrent profiles.
6. **`evaluate_javascript` engine→chrome push** is a string-eval channel; keep it
   strictly JSON-typed and escape rigorously (current `js_string` is the seed) to
   avoid injection from page-derived titles/URLs into the chrome context.

**Open questions:**

- Does Servo at `ed1af70` support **Shadow DOM + custom elements** well enough for
  a web-components chrome (Lit)? Needs a spike before picking the chrome
  component model. If not, the hand-rolled-signals path is mandatory.
- Is the **`ProtocolHandler`** path mature enough at this rev to host
  `navgator://settings` etc. and to carry chrome→engine commands, or do we keep the
  fragment-intercept bridge for v1?
- **Lyku** protocol shape: CRDT vs last-writer-wins per record? E2E key
  management/recovery? Self-host server stack? (Out of this dimension's scope but
  gates `navgator-sync`'s data model — keep records in `navgator-protocol`.)
- **Incognito** semantics: `temporary_storage = true` Servo instance — confirm it
  fully isolates cookies/cache and leaves no `config_dir` residue.
- Multiprocess timeline: when (if) to adopt Servo's `multiprocess`/`sandbox` for
  content, and whether that forces the engine out of the app process anyway.

---

## 11. Sequenced plan (architecture milestones)

| Phase | Deliverable | Unblocks |
| --- | --- | --- |
| **A0** | Workspace split: extract `navgator-servo`, `navgator-protocol`, `navgator-app` from `main.rs`; promote `IpcCommand`→`EngineCommand`. No behavior change. | the firewall; parallel work |
| **A1** | `navgator-data` (SQLite places DB) + `navgator-settings` + per-profile `config_dir`. | history/bookmarks/omnibox/settings |
| **A2** | Typed chrome protocol (`navgator://` handler or structured fragment + JSON); TS build via esbuild; signals store. | a real chrome app |
| **A3** | Full `WebViewDelegate`: dialogs, permissions, `request_create_new` (real new-tab), favicons, downloads, context menus, IME. | "feels like a browser" |
| **A4** | `navgator-theme` + chrome customization studio; `UserContentManager` userstyles/blocking. | the differentiator |
| **A5** | `navgator-testkit` (WebDriver UI suite + Xvfb smoke + WPT) wired into CI; `xtask bump-servo` gate. | sustainable churn-handling |
| **A6** | `navgator-updater` + signed channels; macOS/Windows ports (named-pipe IPC, packaging). | shippable cross-platform |
| **A7** | `navgator-sync` (Lyku) over `navgator-protocol`; self-host server later. | settings/data sync |
| **B+** | Content sandbox/multiprocess; out-of-process engine + stable wire protocol (the Tauri/external-engine goal). | security + reuse |

---

### Sources

- Servo embedding & 2026 multiprocess/sandbox status (Linux/macOS only, off by
  default; IPC-based engine API direction):
  [servo.org](https://servo.org/),
  [Building a browser using Servo](https://servo.org/blog/2024/09/11/building-browser/),
  [servo/servo](https://github.com/servo/servo),
  [constellation sandboxing.rs](https://github.com/servo/servo/blob/main/components/constellation/sandboxing.rs)
- Verso / Tauri-Servo external-engine model:
  [tauri-runtime-verso](https://github.com/versotile-org/tauri-runtime-verso),
  [Experimental Tauri Verso Integration](https://v2.tauri.app/blog/tauri-verso-integration/),
  [NLnet — Servo Webview for Tauri](https://nlnet.nl/project/Tauri-Servo/)
- Local ground truth: cached Servo tree at
  `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`
  (`components/servo/lib.rs`, `site_data_manager.rs`, `network_manager.rs`,
  `user_content_manager.rs`, `webview_delegate.rs`, `servo.rs`,
  `components/config/opts.rs`, `ports/servoshell/*`), and navgator repo
  (`src/main.rs`, `src/chrome/*`, `Cargo.toml`, `Cargo.lock`, `docs/ARCHITECTURE.md`).
