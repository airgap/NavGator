# NavGator master roadmap

Current status + forward plan. Updated **2026-06-20** against the `dev` branch, the
maintained Servo fork (`airgap/swervo`) pinned at rev `33abd9` (carrying the UA + downloads
patches), stable Rust 1.95, a ~940-package dep graph, and a native-egui-chrome shell at
~3,980 lines (`crates/navgator/src/main.rs`).

This document is deliberately unhyped: where a thing is engine-blocked, multi-year, or
not yet feasible it says so. **§R2 (owner decisions) records the locked strategy —
maintained fork, tri-platform, full-web-rendering ambition — that still governs the
plan.** §1 is the current shipped state; §2 is what's next; §3–§6 carry the deeper
engine/security/sustainability framing that remains valid. The ten dimension deep-dives
live in [`docs/plan/`](plan/).

---

## 1. Where NavGator is today (shipped to `dev`)

NavGator is a working web browser with a **native egui chrome composited over Servo**.
Servo renders only page content into an offscreen GL texture; the toolbar, tabs,
dialogs, menus, pickers, and internal pages are drawn directly with egui. This is the
M6 "native-chrome pivot" — it replaced the earlier design where the chrome was a second
Servo webview rendering HTML over a `navgator:`/`gator:` URL bridge.

**The pivot is shipped to `dev` and published to `lyku.org/apps`.** "Security and
performance" is the pitch: native chrome keeps the UI out of the web engine (a cleaner
privilege boundary — privileged actions are direct Rust calls, not URL messages from a
web document) and avoids running a second engine document for the UI.

The whole app is one binary crate (`crates/navgator`); all engine types are quarantined
behind the `navgator-engine` facade (`crates/navgator-engine/src/lib.rs`), the only crate
allowed to `use servo::*`. See [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) and
[`docs/FORK.md`](FORK.md).

### 1.1 Shipped product features

All of the following are implemented in `main.rs` and verified in the running app:

- **Tabs** — new / select / close, per-tab title + navigation, loading spinner, a tab
  context menu, and **favicons** decoded into egui textures in the strip.
- **Reopen-closed-tab** (`Ctrl+Shift+T`) — recently-closed URLs are stacked and popped.
- **Omnibox** — URL-vs-search heuristic with a per-profile **search-engine picker**
  (DuckDuckGo / Kagi / Bing / Google, `%s` templates) and **autocomplete suggestions**
  ranked from history.
- **JS dialogs** — `alert` / `confirm` / `prompt` as native egui modal overlays
  (un-spoofable, drawn by the chrome, not the page).
- **Form controls** — `<select>` element picker, `<input type=file>` (via
  `egui-file-dialog`), and `<input type=color>` picker.
- **Context menu** — native right-click menu.
- **Permission prompts** — `request_permission` wired for Geolocation / Notifications
  (and the rest of `PermissionFeature`); decisions surface as a prompt.
- **HTTP authentication** — Basic / proxy `401` prompt via `request_authentication`.
- **Popups** — `window.open` / `target=_blank` open as new tabs
  (`request_create_new`).
- **Find-in-page** (`Ctrl+F`) — a floating find bar with next/prev stepping, driven by
  injected JS (the engine exposes no native find primitive at this rev — see §2.2).
- **History + bookmarks** — a persisted `Profile` store written as **TSV** under the
  config dir (`config_file()`, `save_history()`, `save_bookmarks()`, `record_visit()`).
  Bookmarks render as quick-link tiles on the new-tab page and as a bookmarks bar.
- **Status bar** — hovered-link URL / status text from `notify_status_text_changed`.
- **Page zoom** — `Ctrl +/-/0` and `Ctrl+wheel`.
- **Page fullscreen** — page-requested fullscreen + `Esc` to exit.
- **Standard keyboard shortcuts** — the nav/tab/zoom/find set, handled in the
  winit/egui key path.
- **Deep theming (v1)** — a configurable **accent color + dark mode**, applied to the
  native chrome and pushed to content via Servo's theme signal; settings persist via
  `save_settings`.
- **Settings persistence** — `Settings` saved to a config file; `Profile` (history +
  bookmarks) persisted as TSV.
- **Borderless window** with resize-from-edges.
- **Opt-in IPC control socket** (`NAVGATOR_IPC`) — an external process can drive
  navigation / tabs and read state events.
- **Session restore** — the open-tab set is persisted (`session.tsv`) and restored on
  launch; an explicit CLI URL takes precedence.
- **Crash tab** — `notify_crashed` → a `gator://crash` sad-tab page with the failed URL
  and a reload-back link.
- **Lyku sync** (early access) — bookmarks + history sync to the user's Lyku account via a
  blocking HTTPS+JSON client on a background thread, with per-collection opt-ins; merge is
  last-write-wins by mtime. The server endpoints (`/sync-push`, `/sync-pull`, a `syncItems`
  table) are built on the Lyku side. *(Live round-trip pending the Lyku deploy.)*
- **Downloads** — files stream to `~/Downloads` with a click-to-open toast and a
  `gator://downloads` manager (`Ctrl+J`). This required **NavGator's first carried
  Servo-fork patch** (a `Content-Disposition`→disk streaming path + a `Download`
  EmbedderMsg/delegate, `airgap/swervo` @ `33abd9`), since libservo has no download
  delegate — closing the old §2.2 gap.
- **E2EE password manager** — an Argon2id + XChaCha20-Poly1305 credential store
  (`passwords.enc`, zero-knowledge); unlock in Settings; **autofill** on load and **save**
  via a 🔑 toolbar button (both through `evaluate_javascript`, so the credential never
  touches page-readable storage); a `gator://passwords` manager; opt-in E2EE sync to Lyku's
  `passwords` collection (ciphertext only, encrypted on the UI thread).
- **Ad / tracker blocking** — Brave's `adblock-rust` checked in `load_web_resource`:
  **network blocking** (matches intercepted with an empty 204), the **full EasyList +
  EasyPrivacy** (~137k rules, fetched + cached in the background with a weekly refresh and a
  bundled starter list), and **cosmetic element-hiding** (page class/id set → matching hide
  selectors → injected `<style>`). On by default; Settings toggle + session counter.
- **Import from other browsers** — bookmarks (Chrome-family JSON) + bookmarks & history
  (Firefox + Chrome, read-only/immutable SQLite via `rusqlite`) from Settings → Setup,
  deduped. The first-run adoption hook.
- **Default-browser registration** — Settings → Setup writes a `navgator.desktop` launcher
  and registers it via `xdg-settings`/`xdg-mime` (Linux).
- **gator://settings** — a full themed in-page settings surface (search engine, theme presets,
  accent, dark, privacy, sync toggles, import + default-browser actions), each change applied via
  a `?key=value` link. Page-initiated navigation to `gator://` is **denied** (a CSRF guard, so a
  web page can't drive the chrome's internal pages); the omnibox + internal links stay allowed.
- **Tab-strip depth** — horizontal overflow/scroll, **drag-reorder**, and a toggleable
  **vertical-tabs** side-strip (persisted), atop the existing pinning + background-throttling.
- **Userscripts** — Greasemonkey-style: every `*.js` in `~/.config/navgator/userscripts/` is
  injected on all pages via Servo's `UserContentManager` (re-exported through navgator-engine).
- **Password store — OS-keyring auto-unlock** — optionally remember the sync passphrase in the
  OS keyring (Secret Service); auto-unlocks the store on launch, with a graceful fallback when
  the keyring is unavailable.

### 1.2 `gator://` internal pages

A custom **`gator://` internal-page scheme** is the model for all NavGator-served
content. `AppState::load_web_resource` intercepts `gator://` loads and returns embedded
HTML built with the engine `http` types (re-exported through `navgator-engine`):

- **`gator://welcome`** — the new-tab page / NTP, templated with the accent color, search
  engine, and bookmarks as quick-link tiles.
- **`gator://settings`** (the full themed settings surface), **`gator://history`** (recent
  visits), **`gator://downloads`** (download manager), **`gator://passwords`** (the masked
  saved-login manager), **`gator://about`**, and **`gator://crash`** (the sad-tab page) — all
  themed through the same `load_web_resource` match + templating.
- Unknown `gator://` paths return a small "no such internal page" fallback.

### 1.3 Engine-gap work landed (the NavGator default web-feature profile)

NavGator ships a curated, non-default Servo pref profile (`navgator_preferences()`)
that turns Servo's conservative "everything off" into a defensible enabled set —
closing the highest-leverage [`engine-gap.md`](plan/engine-gap.md) tiers:

- **Tier-0 media** — `media-gstreamer` is enabled (desktop), so `<video>`/`<audio>`
  actually decode. This was the top correctness bug.
- **Tier-0 silent-breakers** — IntersectionObserver, adoptedStyleSheets, FontFace (CSS
  Font Loading), Web Animations, VisualViewport, Async Clipboard — all flipped on.
- **Permission-gated APIs exposed** — Permissions, Notifications, Geolocation (grants
  go through the prompt).
- **Tier-1** — **IndexedDB** (rusqlite backend) and **WebGL2** enabled.
- **Second wave** — OffscreenCanvas, HTML Sanitizer API, `execCommand`
  (contenteditable), `navigator.storage`.

Each of these remains a candidate for WPT hardening before it is *marketed* as solid;
enabling ≠ passing WPT (§3).

### 1.4 Android — builds and runs (on the `android` branch)

The full app — native egui chrome **plus** the Servo engine (SpiderMonkey/mozjs, stylo,
webrender) — **cross-compiles, links, and runs on `aarch64-linux-android`**, producing a
**signed, installable APK** (`org.airgap.navgator`, NativeActivity → `android_main`,
minSdk 30, `libc++_shared.so` bundled, INTERNET permission), **emulator-verified**. The
native-chrome pivot is precisely what made the UI layer portable to mobile — egui is
touch-capable via winit, whereas the old HTML-chrome-as-a-Servo-webview would not have
suited a phone. **This work lives on the `android` branch, not yet merged to `dev`:** there
the binary is restructured as a lib (rlib + cdylib) with `desktop_main()` +
`android_main(AndroidApp)` entry points, a Jenkins **Android APK** stage builds the APK, and
the Publish stage registers it at `lyku.org/apps/NavGator`. iOS is out (Apple enforces
WebKit outside the EU). Details in `docs/ANDROID.md` (on that branch). Merging `dev` →
`android` later also brings the new `gator://` pages, which fix the Android home page.

Mobile *polish* — touch-input forwarding to Servo, a touch-sized mobile layout, an
Android media backend, and a Play-Store AAB — is tracked under NEXT (§2.4).

---

## 2. What's next (open work)

Grounded in [`product-features.md`](plan/product-features.md) and
[`engine-gap.md`](plan/engine-gap.md). Ordered by leverage. Items are tagged where they
are **engine-blocked** (libservo at this rev lacks the primitive) vs. **embedder work**
(NavGator can build it now).

### 2.1 Core product features still missing (embedder work)

- **Theming depth** — beyond accent + dark: a token catalog, per-site themes, wallpapers
  (a `gator://`/asset handler), a validated theme package format (`theming.md`), and a
  CI-enforced perf budget measured against the 1.0-feature build.
- **Password-manager depth** — **auto-offer-save** on form submit (a `UserScript`/engine form
  hook) and **importing saved passwords** from other browsers (the keychain/DPAPI-gated
  sub-track). (OS-keyring auto-unlock + the userscript mechanism it builds on now ship, §1.1.)
- **Protocol/deep-link handlers** — `request_protocol_handler` is not yet implemented
  (default-browser registration itself now ships, §1.1).
- **Live-sync hardening** — deploy the Lyku `navgator-sync` server branch for a live
  round-trip, then add delete tombstones, auto-sync (timer/on-change), and an OS-keyring for
  the API key.

### 2.2 Engine-blocked product features (need fork work or a fragile stopgap)

- **Downloads + manager — DONE (the first carried fork patch).** libservo had no download
  delegate, so NavGator added a `Content-Disposition`→disk **streaming** path + a `Download`
  EmbedderMsg/delegate in `airgap/swervo` (@ `33abd9`), surfaced as `~/Downloads` saves + a
  `gator://downloads` manager. v1 limits: attachment-only, no progress %, blank tab after a
  download. A carried patch across every rebase (§5).
- **Find-in-page (robust)** — the shipped `Ctrl+F` is a **JS overlay** and is fragile
  on complex DOMs (shadow DOM, virtualized lists, cross-iframe). A native find API in
  the fork is the clean answer.
- **PDF viewing + print / print-to-PDF** — Servo has no PDF renderer and no print
  path; route via a bundled JS/WASM viewer served from a `gator://` page, then
  print-to-PDF on top.
- **Private/incognito isolation** — needs a verified per-webview isolated,
  non-persisted storage/cookie scope; investigate before promising.
- **IME / composition input** — `InputMethodControl` is delivered but NavGator forwards
  only raw keys, so CJK/dead-key input is broken. Mostly embedder work, but fiddly and
  platform-specific.

### 2.3 Accessibility — blocked on a dependency wall

Servo ships a full AccessKit foundation (`notify_accessibility_tree_update`,
`set_accessibility_active`), but NavGator does **not** activate it. The blocker is the
**egui → accesskit dependency wall**: wiring Servo's AccessKit content tree *and* giving
the native egui chrome its own AccessKit adapter requires aligning egui's accesskit
version with Servo's, which does not currently resolve cleanly. Until that dep wall is
broken, screen readers see nothing. Treated as a must-have-before-1.0 item that is
blocked on dependency alignment, not on NavGator design.

### 2.4 Mobile polish (post-build-works)

- Forward **touch input** to Servo (only mouse is forwarded today).
- A **touch-sized mobile layout** (likely a bottom toolbar/tab bar).
- An **Android media backend** (`media-gstreamer` is desktop-only; Android `<video>`/
  `<audio>` need a separate servo-media backend).
- A **Play-Store AAB** (needs a gradle wrapper; the signed APK already suffices for
  `lyku.org/apps` sideload).

### 2.5 Engine-gap Tier-2 (upstream-in-the-fork; plan, don't promise)

These have no clean near-term path and are honestly flagged as multi-quarter fork work
or (for EME) a business/legal track:

- **EME / DRM** — no `MediaKeys`/`EncryptedMedia` WebIDL exists in Servo at all → no
  Netflix/Disney+/Spotify-web. Per §R2 (D5a) DRM is *in scope via a Widevine/PlayReady
  CDM license* — the one deliberate proprietary dependency — but that is a
  business/legal track plus EME plumbing in the fork, gated behind a build flag. Not
  near-term.
- **WebRTC** — servo-media's backend is immature and off; no calling (Meet/Discord-web/
  Jitsi) yet.
- **HTTP/3 / QUIC** — absent; H1+H2 only (functional, slower on Google/CDN traffic).
  Not a correctness blocker.
- **Passkeys / WebAuthn** — `PublicKeyCredential`/authenticator WebIDL is entirely
  absent and `dom_credential_management_enabled = false`; multi-quarter fork work, a
  real and growing login-breaker. Lean on saved passwords + import meanwhile.
- **State partitioning / anti-fingerprinting** — Servo's cookie partitioning is a
  literal `// TODO` stub; CHIPS + fingerprint resistance are fork work, not a 1.0
  differentiator.
- **Service Workers** (PWA/offline) and **Shared Workers / Worklets / WebGPU** — present
  but off; enable + harden behind a WPT gate as capacity allows.
- **Layout long tail** — anchor positioning (`unreachable!`), view-transitions /
  subgrid / masonry (`unimplemented!`), vertical/RTL writing modes (panic-guarded).
  Fork-layout work; polyfill/degrade meanwhile. RTL i18n is gated on the writing-mode
  work.

### 2.6 Survival infrastructure (ongoing)

- **CI lanes** — Jenkins builds tri-platform on `dev` today (the Android APK lane is on the
  `android` branch). Add a **canary
  lane** against fork-HEAD so engine-API breaks surface before a rebase, and a
  **top-sites compat smoke-suite** (not just WPT) on that lane.
- **Maintained-fork cadence** — scheduled rebase/merge of upstream Servo into the
  fork (`scripts/sync-forks.sh`), patches carried on top (§R2 D1a, [`FORK.md`](FORK.md)).
- **Security hardening track** — sandboxed multiprocess content, signed auto-update,
  Safe-Browsing-equivalent. Release-blocking for a safety-claiming public 1.0; the
  single most-likely-to-slip deliverable (§5).

---

## R2. Owner decisions — locked (authoritative strategy)

Self-funded, with staffing, the owner chose the maximal-ambition path. These decisions
govern the whole plan and are why the engine is a **fork**, not an upstream embedding.

| # | Decision | Consequence (straight) |
|---|----------|------------------------|
| D1 | **Fork Servo; implement everything ourselves; do NOT file upstream.** | NavGator is an **engine vendor**, not an embedder — owning layout, SpiderMonkey/JS, net, media, and security patches is the single largest cost. **Locked: maintained fork** — pin to our own Servo fork, rebase/merge upstream on a scheduled cadence, patch on top. "No upstreaming" is the policy; a periodic merge cadence is still required or the fork rots. |
| D2 | **Linux, macOS, Windows first-class from day one.** | All three sandboxes (Linux seccomp+userns, macOS Seatbelt, Windows AppContainer + job objects) and tri-platform GPU/`surfman`/ANGLE bring-up are on the critical path. ~3× the platform/security/CI surface; most of the per-platform sandboxing we build in the fork. **Android now also builds + runs** (§1.4), ahead of the original "post-1.0" framing. |
| D3 | **No cryptocurrency/web3 wallet.** Keep the password manager with **opt-in E2EE sync.** | The crypto *wallet* (Brave/Opera-style bloat) is an explicit non-goal; the zero-knowledge password vault stays and syncs opt-in via the Lyku account ([`sync-lyku-integration.md`](plan/sync-lyku-integration.md)). |
| D4 | **Self-funded.** | A public, safety-claiming 1.0 is not grant-blocked. Money ≠ instant expertise: the pool of Servo-fork + SpiderMonkey + browser-internals engineers is narrow, so **hiring is the critical-path constraint** and bus-factor persists until the team exists. |
| D5 | **Target full web rendering (Chrome-ish parity).** | The engine-blocked features (passkeys, partitioning, downloads/find APIs, WebGL2, service workers, WebRTC) move **in-scope, built in our fork**. Reality: Servo is ~62% WPT / ~20% Baseline-Widely-Available today; closing to parity is historically thousands of engineer-years — a multi-year company mission, sequenced most-used-first against a real top-sites corpus, not a single release. **DRM/EME in scope** via a Widevine/PlayReady **CDM license** (a business/legal track + in-fork EME plumbing; the CDM binary is proprietary — the one deliberate exception to engine independence, gated behind a build flag). |
| D6 | **Resources available (funding + staffing).** | Phase sequencing holds; durations compress with headcount. The defining, dominating cost is the **engine-ownership (D1) + parity (D5) + multi-platform (D2)** combination. |

**Net:** NavGator is **"an independent full web platform + browser + browser company, on
a Servo fork, across desktop + Android."** The most ambitious undertaking in consumer
software — internally coherent given real, sustained resources. The dominating work item
is the engine fork; the gating constraint is hiring engine-capable engineers.

---

## 3. Engine gap analysis (condensed). Full: [`engine-gap.md`](plan/engine-gap.md)

Servo's **cores are production-grade**: Stylo 0.18 (Firefox's CSS cascade),
SpiderMonkey mozjs_sys 140.x (full JIT), WebRender 0.69 — CSS parse + JS exec are at/
near Chrome. The gap is everywhere *between* the cores: Servo's young layout engine, the
implemented-but-disabled DOM subsystems, and the truly-absent subsystems.

**NavGator has closed the cheap end of this gap** (§1.3): media on, the silent-breaker
DOM APIs on, IndexedDB + WebGL2 on. What remains:

| Gap | Severity | Status / plan |
|-----|----------|---------------|
| Media (`<video>`/`<audio>`) | P0 | **DONE** — `media-gstreamer` enabled (desktop). Android backend still needed. |
| IntersectionObserver, adoptedStyleSheets, FontFace, WebAnim, VisualViewport, AsyncClipboard | P0 | **DONE** — enabled; WPT-harden as capacity allows. |
| IndexedDB, WebGL2 | P1 | **DONE (enabled)** — harden behind a WPT gate. |
| Permissions / Notifications / Geolocation | P0/P1 | **DONE (exposed + prompted).** |
| AccessKit a11y | P0 compliance | **Blocked on the egui→accesskit dep wall** (§2.3). |
| Downloads | P0, no libservo primitive | **DONE** — carried fork patch (streaming to disk + `Download` delegate). |
| Find-in-page (robust) | P0, no libservo primitive | Ships as a fragile JS overlay; a native find API is **fork work**. |
| Ad / tracker blocking | product, embedder | **DONE** — `adblock-rust`: network + full EasyList/EasyPrivacy + cosmetic. |
| Service Workers, Shared Workers, Worklets, WebGPU | P1/P2, off | **Enable + harden** later. |
| Cookie/state partitioning (CHIPS) | P2, **engine stub** | **Fork work**; not a 1.0 differentiator. |
| Passkeys/WebAuthn | P2, **WebIDL absent** | **Fork work** — multi-quarter, not a flip. |
| Anchor positioning / subgrid / view-transitions / vertical-RTL writing modes | layout-disabled | **Fork-layout work**; polyfill/degrade meanwhile. |
| EME/DRM (MediaKeys absent) | dealbreaker for streaming | **In scope via CDM license** (D5a); business/legal + in-fork plumbing, build-flag-gated. |
| WebRTC | off, immature | **Defer**; fork work later. |
| HTTP/3/QUIC | absent (H1+H2 only) | **Accept** H2 fallback; not a correctness blocker. |
| WebExtensions/MV3 + CDP/DevTools | absent | **Workaround** — native add-ons + WebDriver + Firefox-RDP, not parity. |

**Quantitatively:** ~62% overall WPT; **87 of 439 categorized Baseline-Widely-Available
features at production quality = 19.8%** (333 partial = 75.9%, 19 unsupported = 4.3%;
**593 in the BWA catalog total** — 593 is the catalog size, *not* the percentage
denominator; 87/593 would be 14.7%). An external linear projection puts the plateau near
~80% by ~2037 — directional, 11 years out, not authoritative. **Every pref-enable must
clear a NavGator WPT bar** — the value-add over raw Servo is a hardened, verified default
profile, not Servo's everything-off defaults.

---

## 4. Subsystem design summaries

### Theming — the headline. Full: [`theming.md`](plan/theming.md)
The native egui chrome makes a theme apply a direct Rust call (no engine round-trip).
NavGator already ships accent + dark (§1.1); the depth play is a token catalog, per-site
themes, wallpapers, a validated package format, and a CI-enforced perf budget. Content
theming rides Servo's theme signal + `UserContentManager`; constraints: injected
stylesheets need a reload (preview-then-persist), `@-moz-document` is disabled (per-site
is embedder-driven by URL), and `backdrop-filter` is not wired to the display list
(build on `filter`/gradients). The differentiator is **measured performance** vs Opera
GX's 650MB–1.2GB / 80–100% CPU — measured against the 1.0-feature build, not the
prototype.

### Sync (Lyku) — the trust feature. Authoritative: [`sync-lyku-integration.md`](plan/sync-lyku-integration.md)
**Lyku is real** (lyku.org; Bun + Postgres + Redis + OpenSearch + NATS + R2) and already
ships an opaque-session-token + `lyk_`-API-key + OAuth2/OIDC auth surface, an R2
presigned-upload flow, and a generic **`synced<T>` replication framework** — the
delta-sync cursor primitive NavGator needs. Sync rides those rails behind a pluggable
`SyncProvider` trait (Lyku + self-host + local-folder). **Crypto caveat:** Lyku is
passwordless, so zero-knowledge E2EE needs a **separate sync passphrase** (Argon2id →
2-tier key hierarchy → XChaCha20-Poly1305; Lyku stores only ciphertext). The crypto
crates are at RC versions in Servo's graph → one choke-point module, pinned, AAD
binding, property-tested merges, external review before any vault ships. Sync is a
replication layer over a local store — today the local store is TSV (history/bookmarks);
the substrate should grow a sync-record envelope as it moves to a real store.

### Security & sandboxing — the release blocker. Full: [`security.md`](plan/security.md)
The native-chrome pivot already removed the worst pre-pivot hazard: a privileged
`file://`/HTML chrome receiving web-controlled strings. Privileged actions are now direct
Rust calls. The remaining load-bearing pre-release requirement is **sandboxed
multiprocess content** (≥ process-per-registered-domain) — the single most-likely-to-slip
deliverable. The engine code exists in Servo but is OFF, partly stubbed, and built on the
unmaintained gaol 0.2.1; the spawn code lives in Servo's constellation, so a pluggable
sandbox is fork work. 1.0 bar: sandboxed multiprocess on Linux x86-64, on by default,
with macOS/Windows post-1.0. Network security is sound and inherited (rustls+aws-lc-rs,
HSTS, CSP, CORS, SRI). Residual: no OOPIF / in-broker net+cookies = below Chrome's
isolation bar; documented, never claimed as near-Chrome safety.

### Extensions & content blocking — the MV3-proof win. Full: [`extensions.md`](plan/extensions.md)
First-class content blocking needs no fork — and **now ships** (§1.1): `load_web_resource`
(per-request interception) + Brave's `adblock` crate give network blocking + the full
EasyList/EasyPrivacy + cosmetic element-hiding. WebExtensions parity is **not** realistic
(multi-person-year, a fork-magnet). Remaining path: **userscripts** (native add-on type via
`UserContentManager`), then a small native `navgator.*` API — deliberately not `chrome.*`.
Default-list redistribution licensing remains a gating legal task.

---

## 5. Sustainability & maintenance strategy. Full: [`sustainability.md`](plan/sustainability.md)

**The Verso lesson:** archived Oct 8 2025 for inability to track Servo's churn while
embedding the **low-level** way (~30 component crates, a 2,200-LOC self-owned
compositor). NavGator embeds the **high-level** way (umbrella `servo` crate, pinned rev,
no compositor) — the architectural insurance is paid. As a **maintained fork** (§R2 D1)
the strategy is process discipline:

1. **CI** — tri-platform builds run on `dev` today (Android APK lane on the `android` branch); add a canary lane (fork-HEAD)
   + a top-sites compat smoke-suite + a pinned dev-container/sccache so the canary is
   cheap enough to actually run.
2. **Scheduled rebase cadence** — merge upstream Servo into the fork on a calendar
   (`scripts/sync-forks.sh`), carry NavGator patches on top. Every "fork feature"
   (downloads, find, sandbox, EME plumbing) is a carried patch — minimize the count.
3. **Quarantine** all `servo::` usage in `navgator-engine`; the binary depends only on
   that facade. Next step: replace the thin re-exports with a servo-free NavGator API
   surface so a Servo type change touches only that one crate.
4. **Honest scope** — "usable not universal", "Chrome parity is a multi-year mission",
   "no robust downloads/PDF/passkeys/RTL yet", and the engine-blocked items flagged in
   §2.
5. **Bus-factor = 1 is the second existential risk** — hiring engine-capable engineers
   gates the safety-critical sandbox + crypto work.

Accept the trades: a maintained fork lags upstream between rebases, and NavGator can
never outrun Servo's web-platform ceiling without funding fork-side feature work.

---

## 6. Ranked risk register

| # | Risk | L × I | Mitigation |
|---|------|-------|-----------|
| 1 | **Fork-maintenance treadmill** (killed Verso); amplified by every fork-side feature patch | Med × Critical | Scheduled rebase cadence, quarantine crate, canary CI, minimize carried-patch count |
| 2 | **Bus-factor = 1** — safety/crypto work can't safely ship solo | High × Critical | Recruit engine-capable engineers; gate the sandbox + vault on it |
| 3 | **Sandbox is the gating, most-likely-to-slip item** (Servo-owned spawn + gaol 0.2.1 + per-platform) | High × Critical | Linux-x86-64-first bar; pluggable-sandbox as fork work; pre-decided Linux-first fallback |
| 4 | **Engine-blocked P0s with no primitive** (downloads buffer-breaks, find overlay fragile) | High × High | Documented stopgaps with limits; robust versions as funded fork work |
| 5 | **Accessibility blocked on the egui→accesskit dep wall** | Med × High | Align egui/accesskit versions; treat as must-have-before-1.0 |
| 6 | **Footprint marketed off the prototype** (now media/IndexedDB/WebGL2 ON) | Med × Med | Re-baseline perf vs the 1.0-feature build in CI before marketing |
| 7 | **Web-compat breakage churns users** (62% WPT, 19.8% BWA) | High × Med | Top-sites smoke corpus + a compat page + open-in-system; honest messaging |
| 8 | **No passkeys** breaks a growing share of logins (Google/MS/Apple/banks) | High × Med | Saved passwords + import + open-in-system; message clearly; fork work to fix |
| 9 | **GPU/driver/surfman init fails on the long tail** — "won't start", invisible to a telemetry-free browser | Med × High | Supported-GPU matrix + llvmpipe fallback + an actionable `gator://` error page |
| 10 | **Legal/licensing gate missed** (filter lists, SB feeds, trademark, GDPR, Widevine CDM) | Med × High | Cleared per item before the binary ships; CDM is build-flag-gated |
| 11 | **Crypto correctness on RC crates** (nonce reuse / lost passphrase is silent + catastrophic) | Low × Critical | One choke-point module, pinned crates, AAD binding, property-tested merges, external review before any vault ships |
| 12 | **Scope creep toward Chrome parity** burns the team | Med × High | Sequence most-used-first against a real top-sites corpus; enforce non-goals |
| 13 | **Build weight** (~940 deps, mozjs/ANGLE, LLVM pinning) taxes CI + onboarding + package size | High × Med | sccache, fat caches, dev container; package-size as a CI metric |
| 14 | **Servo upstream loses funding / slows** (Igalia-dependent) | Low-Med × Critical | Maintained fork absorbs it; fund fork-side feature work; accept inheritance |

---

## 7. NavGatorOS note. Full: [`swerveos.md`](plan/swerveos.md)

A Rust OS with NavGator as primary UI is conceivable (Servo runs on Redox) but its cost
is dominated by **kernel + drivers**, not the browser (~5–10% of the effort). ChromeOS
and webOS both run on the Linux kernel precisely to inherit drivers; a from-scratch Rust
OS discards that one shippability asset. **Recommendation: zero engineering spend now** —
keep it as a free narrative. If ever built, make it a **NavGator-as-shell immutable Linux
kiosk image** that reuses Linux drivers, only for captive/known hardware.
