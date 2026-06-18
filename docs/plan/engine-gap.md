# Servo engine capability gap vs Chrome/Blink

**Scope:** Web-platform capability of Servo (libservo, pinned rev `ed1af70`) measured against a production engine (Blink/Chrome, Gecko/Firefox), to decide what swerve must wait-for, upstream, fund, or work around, and which gaps are dealbreakers for "industry standard."

**Method:** Ground truth from the cached Servo source at `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`, the pinned `Cargo.lock`, the feature-gate registry in `components/config/prefs.rs`, and live 2026 data from `servo.org/wpt`, `wpt.fyi`, and `webtransitions.org/servo-readiness`.

---

## 0. TL;DR (the one-paragraph verdict)

Servo's **engine cores are not the problem** — it ships Mozilla's real production CSS engine (**Stylo 0.18**, the same `stylo` crate Firefox uses) and Mozilla's real JS+Wasm engine (**SpiderMonkey, `mozjs_sys` 140.x = Firefox-140-class, full Baseline+Ion JIT**), plus **WebRender 0.69** for GPU compositing. So CSS *parsing/cascade* and *JavaScript execution* are at or near Chrome parity. The gap is everywhere **between** those cores: Servo's own **layout** engine (a young, parallel, fragment-tree reimplementation — not Gecko's) is missing or stubbing big swaths of modern CSS (anchor positioning, view-transitions, subgrid, masonry, masks, vertical/RTL writing modes, multicol maturity); whole **subsystems are implemented-but-disabled-by-default** (IndexedDB, Service Workers, Shared Workers, WebGL2, WebGPU, WebRTC, WebVTT, Web Animations, OffscreenCanvas, Permissions, Geolocation, Notifications, CSS Font Loading, accessibility, devtools); and several **non-negotiable browser subsystems are absent**: **no EME/DRM** (no Netflix/Spotify/Disney+), **no HTTP/3/QUIC**, **media only via the optional GStreamer backend** (off in swerve today → effectively *no `<video>`/`<audio>`*), **devtools is a Firefox-RDP server, not Chrome CDP**, and there is **no extension engine**. Quantitatively: **~62% overall WPT** (up from 30% in ~2.5 yr) but only **19.8% of "Baseline Widely Available" features at production quality** (87/439 fully, 333 partial, 19 unsupported), with an honest external projection that at ~13 FTE Servo "plateaus around 80% by ~2037 and never catches up." **The maintenance treadmill and the long-tail capability gap — not any single missing feature — are the strategic risk.**

---

## 1. The stack swerve actually inherits (exact versions at `ed1af70`)

From the pinned `Cargo.lock`:

| Layer | Component | Version @ ed1af70 | Provenance / parity |
|---|---|---|---|
| CSS cascade/parse | `stylo` | 0.18.0 (git rev `49e912c`) | **Firefox's actual style system** (Servo project owns stylo; Gecko consumes it). Cascade, selectors, custom props, `@supports`, media/container queries = production-grade. |
| JS + Wasm | `mozjs_sys` | 140.12.0 (SpiderMonkey, esr140 branch) | **Firefox 140's JS engine**, full JIT (`js_jit` is a *default* feature). ECMAScript conformance ~Firefox. |
| Compositor | `webrender` | 0.69.0 | Mozilla's GPU display-list renderer (same lineage Firefox ships). |
| WebGL backend | `mozangle` | 0.5.5 (ANGLE) | Real ANGLE; GLES→native. WebGL1 works; **WebGL2 gated off** (see §3). |
| WebGPU backend | `wgpu-core` | 29.0.3 | Real `wgpu`; **gated off by default** (see §3). |
| Layout | `servo/components/layout` | (in-tree) | **Servo's own** "layout 2020" fragment-tree engine + **Taffy** for flexbox/grid. This is the youngest, leakiest part. |
| Media | `servo-media` (+ GStreamer) | in-tree | Pipeline is real but **codecs come from GStreamer**, which is a *non-default* feature (`media-gstreamer`). swerve does not enable it today. |
| Net | `hyper` 1.x + `hyper-rustls` + `tungstenite` | in-tree | HTTP/1.1 + HTTP/2 + WS only. **No HTTP/3/QUIC.** |
| Storage | `rusqlite` (bundled SQLite) | in-tree | IndexedDB now has a real SQLite backend (gated off); cookies + localStorage real. |
| A11y | `accesskit` | workspace | AccessKit tree plumbed through layout/constellation; **`accessibility_enabled = false`** by default. |
| DevTools | `components/devtools` | in-tree | **Firefox Remote Debugging Protocol**, "reverse-engineered from Firefox devtools logs." **Not** Chrome DevTools Protocol. |

**Implication for swerve:** the parts that would be hardest to build (a conformant CSS cascade, a JIT JS engine, a GPU compositor) are *already production-grade and maintained by Mozilla/Servo*. swerve's parity fight is almost entirely in (a) Servo's bespoke **layout**, (b) flipping on and hardening the **disabled DOM subsystems**, and (c) the **truly-absent** subsystems (EME, HTTP/3, CDP, extensions).

---

## 2. The feature-gate registry: what's on vs off by default

`components/config/prefs.rs` documents the canonical feature list (`// feature: X | #issue | MDN`) and `const_default()` sets the shipping defaults. This is the single most useful capability map in the tree. Distilled:

### Enabled by default (works out of the box)
`AbortController`, `MutationObserver`, `ResizeObserver`, `WebCrypto subtle` (incl. post-quantum ML-KEM/ML-DSA — Servo is *ahead* of Chrome here), `Gamepad`, `ClipboardEvent`, parallel CSS parsing, `crypto.subtle`, WebXR (desktop glwindow), `dom_canvas_text`.

### Implemented but **OFF by default** (must be enabled + hardened)
| Feature | pref | Notes |
|---|---|---|
| **IndexedDB** | `dom_indexeddb_enabled=false` | Real `rusqlite` backend exists; off. **Dealbreaker if off** (every modern PWA/app uses it). |
| **Service Workers** | `dom_serviceworker_enabled=false` | Manager + constellation wiring exist; off. Needed for PWAs/offline. |
| **Shared Workers** | `dom_sharedworker_enabled=false` | off. |
| **WebGL2** | `dom_webgl2_enabled=false` | off (WebGL1 on). Breaks many WebGL apps/games/maps. |
| **WebGPU** | `dom_webgpu_enabled=false` | `wgpu-core 29` present; off. |
| **WebRTC** | `dom_webrtc_enabled=false` | servo-media webrtc backend; off. **No video calls / Meet / Discord-web.** |
| **Web Animations API** | `dom_web_animations_enabled=false` | off. |
| **WebVTT** (captions) | `dom_webvtt_enabled=false` | off. |
| **OffscreenCanvas** | `dom_offscreen_canvas_enabled=false` | off. |
| **IntersectionObserver** | `dom_intersection_observer_enabled=false` | off — **surprising**, lazy-loading/infinite-scroll libs assume it. |
| **CSS Font Loading API** (`FontFace`) | `dom_fontface_enabled=false` | off — web-font-driven sites degrade. |
| **adoptedStyleSheets** | `dom_adoptedstylesheet_enabled=false` | off — modern web-components/Lit/Shadow DOM styling. |
| **Permissions API** | `dom_permissions_enabled=false` | off. |
| **Geolocation** | `dom_geolocation_enabled=false` | off. |
| **Notifications** | `dom_notification_enabled=false` | off. |
| **CookieStore** | `dom_cookiestore_enabled=false` | off. |
| **Credential Management / WebAuthn-adjacent** | `dom_credential_management_enabled=false` | off — passkeys/password managers. |
| **HTML Sanitizer API** | `dom_sanitizer_enabled=false` | off. |
| **execCommand** (rich-text editing) | `dom_exec_command_enabled=false` | off — contenteditable editors. |
| **VisualViewport** | `dom_visual_viewport_enabled=false` | off — mobile/zoom-aware sites. |
| **Async Clipboard** | `dom_async_clipboard_enabled=false` | off (sync ClipboardEvent on). |
| **File & Directory Entries** | `dom_entries_api_enabled=false` | off. |
| **Storage API** (`navigator.storage`) | `dom_storage_manager_api_enabled=false` | off. |
| **Worklets** | `dom_worklet_enabled=false` | off — Paint/Audio/Layout worklets, Houdini. |
| **Accessibility tree** | `accessibility_enabled=false` | off — see §7. |
| **DevTools server** | `devtools_server_enabled=false` | off; and it's Firefox-RDP anyway. |
| **CSS layout gates** | `layout_grid_enabled`, `layout_columns_enabled`, `layout_writing_mode_enabled`, `layout_container_queries_enabled`, `layout_variable_fonts_enabled`, `layout_css_attr_enabled` | These are *stylo* gates flipped via `set_pref!`; some default on, some experimental. |

**Why this matters for swerve:** many of these are a single pref flip away from being *available* — but "available" ≠ "passes WPT." Several are off precisely *because they're not solid yet* (regressions, crashes, low conformance). swerve's job is to (a) decide a default pref profile distinct from Servo's conservative one, and (b) **own a test gate**: flip a feature on only when its WPT subscore clears a swerve-defined bar, else you ship bugs users blame on swerve, not Servo.

---

## 3. CSS / layout coverage and gaps

CSS **parsing and cascade** is Stylo → effectively Firefox-grade (custom properties, `@supports`, `@container`, `@layer`, nesting, color-mix, relative colors, math functions all parse). The gap is in **what Servo's layout engine does with the parsed values.**

### Confirmed gaps (from source, not speculation)
- **Anchor positioning** — parsed but hard-disabled. `components/layout/style_ext.rs`: `Inset::AnchorFunction(_) => unreachable!("anchor() should be disabled")` and `unreachable!("anchor() and anchor-size() should be disabled")`. **No anchor positioning.**
- **View transitions** — `components/script/dom/css/cssrule.rs`: `StyleCssRule::ViewTransition(_) => unimplemented!() // TODO` (twice). The script thread has the spec hook stubbed. **No `@view-transition` / `document.startViewTransition`.**
- **Subgrid + masonry** — `components/layout/taffy/stylo_taffy/wrapper.rs`: `GenericGridTemplateComponent::Subgrid(_) => None` with `// TODO: Implement subgrid and masonry`. Grid runs on **Taffy**; subgrid and masonry are unimplemented.
- **Vertical / RTL writing modes** — repeated hard asserts: `flow/mod.rs` `"Vertical writing modes are not supported yet"`, `positioned.rs` ×3 `"Mixed horizontal and vertical writing modes are not supported yet"`, `geom.rs` `// TODO: Bottom-to-top and right-to-left vertical writing modes are not supported yet`. CJK vertical text and many RTL layouts **break or panic-guard**. `layout_writing_mode_enabled` is the gate.
- **Gradient color hints** — `display_list/gradient.rs` ×2: "Remove color transition hints, which are not supported yet." Mid-point gradient hints dropped.
- **Multicol** — gated (`layout_columns_enabled`); historically immature (orphans/widows/spanning).
- **`text-overflow: ellipsis`, `user-select`, CSS masks (`mask-image`), clip-path maturity** — the prompt's hint about `layout.unimplemented`: that is a **stylo pref** (`set_pref!("layout.unimplemented", …)`) that gates *parsing* of properties Servo can't lay out, so they don't even apply. No bespoke ellipsis/user-select handling found in `components/layout/display_list/`. Treat these as **partial/absent** until proven by WPT.

### Quantified
- **Overall WPT ~62%** (servo.org/wpt; "doubled from 30% to 62% over ~2.5 years").
- **CSS specifically** is one of Servo's *stronger* suites due to Stylo, but layout-dependent CSS subtests drag it down vs Chrome's ~95%+.
- Baseline readiness (webtransitions.org): **19.8% of Baseline-Widely-Available features fully supported**; **75.9% partial**; **141 features at zero progress** "require architectural improvements."

### swerve posture
- **Must wait-for / upstream:** anchor positioning, view-transitions, subgrid, masonry, vertical writing modes. These are *layout-architecture* work only the Servo layout team (or a funded swerve contributor) can do. Do **not** try to fork layout.
- **Work-around:** detect-and-degrade — e.g. polyfill `startViewTransition` to a no-op crossfade; ship a default UA stylesheet that avoids relying on ellipsis where swerve chrome is concerned.
- **Dealbreaker tier:** none individually fatal, but the *aggregate* of "site uses subgrid + anchor tooltips + view-transition nav" = visibly-broken vs Chrome on modern sites.

---

## 4. JavaScript / ECMAScript

**Strongest area.** SpiderMonkey `mozjs_sys` 140.x with `js_jit` **on by default** (Baseline interpreter + Baseline JIT + IonMonkey + Wasm Baseline/Ion). ES2023/2024 language features, async/await, generators, BigInt, WeakRefs, top-level await, modules, import maps, Wasm (incl. SIMD, threads where wired) are SpiderMonkey-grade. The ECMAScript test262-style WPT `js/` area is high.

**Caveat:** the *engine* is production-grade; the *bindings surface* (which Web APIs are exposed to JS) is the gate — see §2/§5. So `Array.prototype.*` is fine, but `navigator.serviceWorker` may be `undefined` because the **API is pref-off**, not because JS is weak. swerve users will perceive these as "JS errors" (`TypeError: x is not a function`) — a UX problem, not an engine problem.

**Recommendation:** no engine work needed. swerve should expose a curated, *enabled* global surface and ensure feature-detection paths degrade (sites that `if ('serviceWorker' in navigator)` will simply not use it).

---

## 5. DOM / HTML APIs

**539 WebIDL interfaces** ship in `components/script_bindings/webidls/` — a large, real DOM. HTML parsing (`html5ever`), DOM core, events, `fetch`, `XMLHttpRequest`, `FormData`, `URL`, `TextEncoder`, structured clone, `BroadcastChannel`, `EventSource`, `MessageChannel`, Shadow DOM, custom elements, `<template>`, `<canvas>` 2D (vello/vello_cpu backend) are present.

**Gaps:** the pref-off list in §2 *is* the DOM-API gap (IndexedDB, SW, IO, Permissions, Geolocation, Notifications, IntersectionObserver, adoptedStyleSheets, FontFace, OffscreenCanvas, Worklets, execCommand, CookieStore, CredMgmt, Sanitizer, VisualViewport, Async Clipboard). Editing/`contenteditable` is weak (execCommand off). Popover API, `<dialog>` maturity, `inert`, and form-control fidelity should be WPT-spot-checked before relying on them.

**Recommendation:** define a **swerve default pref profile** that turns on the subset that is *both* commonly required *and* WPT-passing at swerve's bar (candidate first wave: IntersectionObserver, ResizeObserver [already on], adoptedStyleSheets, FontFace, Web Animations, VisualViewport, Async Clipboard). Gate the heavy ones (IndexedDB, SW) behind explicit hardening milestones.

---

## 6. Media & codecs (the worst practical gap for a daily driver)

- **Pipeline:** `servo-media` with backends `dummy | gstreamer | ohos | auto`. Real codecs come **only** from the **GStreamer** backend, enabled by the **non-default** `media-gstreamer` Cargo feature.
- **swerve today:** README explicitly leaves `media-gstreamer` OFF ("those libs aren't needed"). **Net effect: `<video>`/`<audio>` decode to nothing / dummy backend.** This is a *current swerve build choice*, not a Servo limitation — but it means **swerve presently has no media**.
- **Codecs (with GStreamer on):** whatever GStreamer plugins are installed — typically H.264/AAC/Opus/Vorbis/VP8/VP9, AV1 via `dav1d`/`aom` plugins, etc. Quality/coverage = the host's GStreamer install, not a fixed guarantee. Recent Servo work added Ogg audio via `<audio>`.
- **EME / DRM:** **absent.** No `MediaKeys` / `EncryptedMedia` / `navigator.requestMediaKeySystemAccess` WebIDL in the tree (grep: zero matches). **No Widevine/PlayReady/FairPlay → no Netflix, Disney+, Spotify-web, Amazon Prime, HBO, most paid streaming.** Servo's own stance (per 2026 discussions) is that a "daily driver doesn't really need EME" and that DRM could be a separate app — i.e. **not on Servo's roadmap.**

### swerve posture — this is a **Tier-1 dealbreaker** for "industry standard"
1. **Enable `media-gstreamer` immediately** (or wire the OHOS/system backend) — without it swerve isn't a usable browser. Accept the native-lib build dependency.
2. **EME:** there is no clean path. Options, all painful:
   - **(a) Out-of-process Widevine CDM** like Chromium/Firefox do — Google licenses Widevine binaries; obtaining a license + sandboxing the `libwidevinecdm.so` and bridging it to a (currently nonexistent) Servo EME implementation is a **multi-quarter, partnership-gated** effort. This is the single largest "fund/partner" item.
   - **(b) Ship without DRM** and message it ("swerve doesn't do DRM streaming; use the native app"). Honest, but a hard sell for "Chrome parity."
   - **(c) Defer** and revisit once Servo has any EME hooks (none today).
   - **Recommendation:** plan (b) for v1, with (a) as a funded later milestone. Do **not** promise Netflix at launch.

---

## 7. Graphics: WebRender / WebGL / WebGPU / Canvas

- **Compositing:** WebRender 0.69 — solid, GPU-accelerated, this is a strength.
- **Canvas 2D:** backed by **vello / vello_cpu** (`dom_canvas_backend` pref) — modern, but verify text/path conformance vs Chrome.
- **WebGL1:** works (ANGLE 0.5.5 via `mozangle`/`surfman`).
- **WebGL2:** **off** (`dom_webgl2_enabled=false`). Implemented to some degree; needs enabling + WPT hardening. Many 3D sites/maps/games require it.
- **WebGPU:** present (`wgpu-core 29`) but **off** (`dom_webgpu_enabled=false`) and behind the `webgpu` Cargo feature. Conformance is early everywhere; treat as experimental.

**Recommendation:** enable WebGL2 behind a hardening gate (high payoff, real implementation exists). WebGPU stays experimental/opt-in. Both depend on the host GL/Vulkan/Metal stack — surfman portability is a known sharp edge (swerve already battles ANGLE/LLVM mismatches per README).

---

## 8. Networking

- **HTTP/1.1 + HTTP/2:** yes (`hyper` with `http1,http2` + `hyper-rustls`). TLS via rustls.
- **HTTP/3 / QUIC:** **absent** (no `quinn`/`h3`/`s2n-quic` anywhere in `components/net`). Modern Google/CDN traffic prefers H3; without it swerve falls back to H2 (functional, slightly slower, more visible to network-fingerprinting). **Not a correctness dealbreaker; a performance/parity gap.**
- **WebSockets:** yes (`tungstenite` / `async-tungstenite`).
- **fetch / CORS / cache:** real `fetch` stack, `cors_cache.rs`, and a real `http_cache.rs` + `image_cache.rs`. Good.
- **Service Worker fetch interception:** depends on SW being enabled (off) — so no offline/PWA caching today.

**Recommendation:** HTTP/3 is a "nice-to-have / upstream-or-fund later." H2 fallback is fine for v1. Prioritize turning the **HTTP cache** correctness up (it gates perceived speed) over chasing QUIC.

---

## 9. Workers & storage

- **Web Workers (dedicated):** present and used.
- **Service Workers:** implemented (manager in `components/script/serviceworker_manager.rs`, constellation wiring, full WebIDL set) but **off**. The hard part (cross-process lifecycle) exists; needs hardening to enable.
- **Shared Workers:** off.
- **Worklets:** off.
- **Storage:** cookies (real, with http-state test suite), localStorage/sessionStorage (`components/storage/webstorage`), **IndexedDB on real bundled SQLite** (`rusqlite`) — but **pref-off**. This is the most consequential storage gate: enabling IndexedDB unblocks a huge class of web apps.

**Recommendation:** sequence = (1) confirm cookies+localStorage solid (likely yes), (2) **harden+enable IndexedDB** (biggest unlock, real backend already there), (3) tackle Service Workers (needed for PWAs and swerve's own offline/Lyku-sync story), (4) Shared Workers/Worklets last.

---

## 10. WebRTC

`dom_webrtc_enabled=false`; servo-media has a webrtc backend but it is **off and immature**. **No real-time video/audio calls** (Meet, Discord web, Whereby, Jitsi) today. This is **Tier-1 for "industry standard"** and **Tier-2 for swerve's likely early users**. Treat as fund/upstream-later; do not promise calling at launch.

---

## 11. Accessibility

AccessKit is plumbed through `components/layout/accessibility_tree.rs`, constellation, and the servo crate — a genuinely modern a11y foundation (AccessKit bridges to AT-SPI/UIA/macOS AX). **But `accessibility_enabled=false` by default**, and the tree is young. For an "industry standard" browser this is a **compliance + ethics + (in some markets) legal** requirement.

**Recommendation:** enable AccessKit, run the a11y WPT + manual screen-reader (Orca/NVDA) passes, and treat a11y as a **must-have for v1**, not a nice-to-have. It's far cheaper to keep an AccessKit tree correct as you grow than to retrofit.

---

## 12. DevTools / CDP

`components/devtools` implements the **Firefox Remote Debugging Protocol** ("reverse-engineered from Firefox devtool logs"), with a `network_handler`, actors, etc. It is **off by default** and is **not Chrome DevTools Protocol (CDP)**.

**Consequences for swerve:**
- swerve cannot reuse Chrome DevTools front-end, Puppeteer, Playwright-chromium, or the vast CDP tooling ecosystem out of the box.
- WebDriver exists (`components/webdriver_server`) — that's the portable automation path.
- Building swerve's own in-browser devtools (Opera-GX-class UX is a selling point) means either (a) talking Firefox-RDP to the existing actors, or (b) implementing a CDP shim over Servo internals (large).

**Recommendation:** for v1, lean on the **Firefox RDP server + WebDriver**; defer any CDP ambitions. A CDP shim is a fundable later project if swerve wants Playwright/Puppeteer compatibility.

---

## 13. Extensions

Not in scope of the engine per se, but worth flagging as a **structural gap**: Servo has **no WebExtensions/MV3 engine**. "Chrome parity" implies ad-blocking/uBlock-class extensions. There is no path to running Chrome/Firefox extensions without building an extension runtime (content-script injection, `webRequest`/`declarativeNetRequest`, background SW, extension storage) on top of Servo. This is a **major, separate, fundable workstream** — but note swerve's anti-bloat thesis could instead favor a **built-in content-blocker** (network-level filter lists via the existing net stack) rather than a full extension engine, which is far cheaper and aligned with the product.

---

## 14. The strategic risk: velocity, not any single feature

External, adversarial measurement (webtransitions.org/servo-readiness, 2026):

- Servo completes **~22 BWA features/year**; the web platform grows **~52 new BWA features/year** → a **structural deficit**.
- **51 features regressed** >5pp; **141 at zero progress**; **154 features lack WPT coverage** (78 JS built-ins, 23 semantic HTML elements with *unknown* status).
- Projection: at **13 FTE**, Servo "plateaus around 80% by ~2037 and never catches up." Velocity parity within 3 years is estimated at **~44 FTE / ~€26.3M**.

This is the **Verso lesson restated at the engine layer**: the danger isn't that Servo can't render the web — it's that the web outruns Servo, and a small embedder (swerve) inherits that gap *plus* the cost of tracking Servo's churn. swerve's architecture (high-level `libservo`, pinned rev, deliberate bumps) correctly addresses the *embedding* treadmill; it does **not** address the *capability* treadmill, which is upstream and largely outside swerve's control.

**Mitigations swerve can actually pursue:**
1. **Fund upstream Servo** for the specific features swerve needs (anchor positioning, IndexedDB hardening, WebGL2, media). Cheaper and more durable than working around in the embedder.
2. **Be honest about scope:** position swerve as a *fast, private, themeable* browser for the *mainstream non-DRM web*, not "renders everything Chrome does." Pick a target site-list and make *those* perfect.
3. **Own a pref/test profile:** swerve's value-add over raw Servo is a *curated, hardened default feature set* gated by swerve's own WPT bar — turning Servo's conservative "everything off" into a defensible "these N features on, verified."
4. **Track the velocity gap as a KPI**, not a vibe: run WPT against each Servo bump, alert on regressions in swerve's target feature set.

---

## 15. Prioritized recommendations (capability roadmap for swerve)

**Tier 0 — without these swerve is not a usable browser (do now):**
1. Enable **`media-gstreamer`** (or system media backend) → `<video>`/`<audio>` actually play. *(Currently OFF — top correctness bug.)*
2. Enable + smoke-test **IntersectionObserver, adoptedStyleSheets, FontFace (CSS Font Loading), Web Animations, VisualViewport** — the silent breakers of modern sites that are cheap to flip.
3. Enable **AccessKit accessibility** — must-have for v1.

**Tier 1 — required for credible "industry standard" (fund/harden, this year):**
4. **Harden + enable IndexedDB** (real SQLite backend exists) → unblocks web apps.
5. **Enable WebGL2** behind a WPT gate → 3D/maps/games.
6. **Enable + harden Service Workers** → PWAs, offline, and swerve's own Lyku-sync offline story.
7. **Async Clipboard, Permissions, Notifications, Geolocation** — common permission-gated APIs.

**Tier 2 — parity gaps that need upstream or partnership (plan, don't promise):**
8. **EME/Widevine** — partnership-gated, multi-quarter; v1 ships *without* DRM and says so.
9. **WebRTC** — fund/upstream; no calling at launch.
10. **HTTP/3/QUIC** — performance parity; upstream later.
11. **Anchor positioning / view-transitions / subgrid / vertical writing modes** — upstream Servo layout work; polyfill/degrade meanwhile.
12. **A content-blocker** (network-level filter lists) instead of a full WebExtensions engine, aligned with the anti-bloat thesis.

**Tier 3 — ecosystem / tooling (later):**
13. CDP shim for Playwright/Puppeteer compatibility (large; RDP+WebDriver suffice for v1).
14. WebGPU (stay experimental/opt-in).

**Hard dealbreakers for literal "Chrome parity"** that swerve must either fund-around or explicitly de-scope: **EME/DRM streaming, WebRTC calling, WebExtensions**. The defensible product is *not* "Chrome parity" — it's "the fast, private, deeply-themeable browser for the open (non-DRM) web," with a funded path to close Tier-1/Tier-2 over time.

---

## 16. Sources

- Cached Servo source @ `ed1af70`: `components/config/prefs.rs` (feature registry + `const_default`), `components/layout/{style_ext.rs,flow/mod.rs,positioned.rs,geom.rs,taffy/...}`, `components/script/dom/css/cssrule.rs`, `components/{net,media,storage,devtools,webgpu,webgl}/Cargo.toml`, `Cargo.lock` (mozjs_sys 140.12.0, stylo 0.18.0, webrender 0.69.0, wgpu-core 29.0.3, mozangle 0.5.5).
- [Servo WPT pass rates](https://servo.org/wpt/) — ~62% overall, "doubled from 30% over ~2.5 years."
- [Servo Baseline Readiness](https://webtransitions.org/servo-readiness/) — 19.8% BWA fully supported (87/439 categorized = 19.8%; 333 partial; 19 unsupported; 593-feature catalog total), velocity deficit, ~44 FTE / €26.3M to reach parity. The headline percentages are over the 439 categorized features; 593 is the catalog total, NOT the percentage denominator (87/593 would be 14.7%). This canonical framing is used across ROADMAP.md and positioning.md. These FTE/€ and 2037-plateau figures are a single external linear projection 11 years out — directional, not authoritative.
- [wpt.fyi Servo runs](https://wpt.fyi/results/?product=servo) — latest run rev `64b3fe9`, Servo 0.3.0, 2026-06-17.
- [Servo: Web Platform Tests (blog)](https://servo.org/blog/2023/07/20/servo-web-platform-tests/), [Phoronix: Servo January 2026](https://www.phoronix.com/news/Servo-January-2026), [servo/mozjs](https://github.com/servo/mozjs).
