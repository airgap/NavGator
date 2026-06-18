# swerve: Extensions, Add-ons & Content Blocking

*Dimension plan. Ground truth verified against the repo (`/raid/swerve`) and the
pinned Servo checkout (`ed1af70`, at
`/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`). June 2026.*

---

## 0. TL;DR / recommendation

1. **Ship content blocking FIRST, as a first-class built-in — not an extension.**
   Servo already gives us everything needed at the embedder level: a per-request
   network interception hook (`WebViewDelegate::load_web_resource`) and a
   per-webview user content injection channel (`UserContentManager::add_script` /
   `add_stylesheet`). Pair these with Brave's `adblock` crate (v0.12.5, the same
   engine that ships in Brave) for network + cosmetic filtering. This is achievable
   in the **low thousands of lines** of swerve code and gives a headline,
   differentiating feature with no Servo fork.
2. **Do NOT attempt WebExtensions parity.** Servo has *zero* WebExtensions
   infrastructure (no `chrome.*`/`browser.*` bindings, no background pages, no
   extension process model, no CRX/XPI loader, no `manifest.json` handling). The
   only effort in the wider ecosystem — the *Moto* browser — is one person
   part-time and is not even tracking upstream Servo. Building a meaningful subset
   of WebExtensions is a **multi-person-year** effort and a fork-magnet — exactly
   the Verso maintenance-treadmill risk we are trying to avoid.
3. **Build a swerve-native add-on model** instead: declarative content-blocker
   lists, Greasemonkey/Tampermonkey-style **userscripts** (the single most-wanted
   capability and the one Servo natively supports today), and a small,
   swerve-owned `swerve.*` JS API surfaced into add-on contexts via the existing
   `swerve:`/UCM machinery. Treat any future WebExtensions support as an optional,
   far-horizon compatibility shim, not a v1 goal.

---

## 1. What Servo gives us today (verified facts at `ed1af70`)

The embedding surface that matters for this dimension lives in the `servo`
umbrella crate (which swerve already depends on) and `embedder_traits`. swerve does
**not** need to fork Servo to do content blocking or userscripts. Concretely:

### 1.1 Network-level request interception — `WebViewDelegate::load_web_resource`

`components/servo/webview_delegate.rs` defines:

```rust
fn load_web_resource(&self, webview: WebView, load: WebResourceLoad) {}  // default no-op
```

The flow (verified):

- Inside the net thread, `components/net/fetch/methods.rs::main_fetch` calls
  `context.request_interceptor.lock().await.intercept_request(...)` **for every
  request** (`methods.rs:537`).
- `components/net/request_interceptor.rs` packages each request into a
  `WebResourceRequest` and sends `NetToEmbedderMsg::WebResourceRequested(webview_id,
  req, sender)` to the embedder, then **awaits** a `WebResourceResponseMsg` reply.
- `components/servo/servo.rs:390` drains that message in `spin_event_loop` and calls
  our `WebViewDelegate::load_web_resource(webview, WebResourceLoad)`.

`WebResourceRequest` (`components/shared/embedder/lib.rs:673`) carries exactly what a
filter engine needs:

| field | type | use for filtering |
| --- | --- | --- |
| `url` | `Url` | the request URL to match against filter lists |
| `method` | `Method` | rarely needed |
| `headers` | `HeaderMap` | referrer/origin, `Sec-Fetch-*` |
| `destination` | `Destination` | maps to adblock request "type" (see below) |
| `referrer_url` | `Option<Url>` | the **first-party** / document URL for filtering |
| `is_for_main_frame` | `bool` | never block the top-level document |
| `is_redirect` | `bool` | re-check after redirects |

`Destination` (from `content-security-policy 0.8.0`) has the full set we need to
map to adblock-rust request types: `Document, Image, Script, Style, Font, Frame,
IFrame, Object, Media (Audio/Video/Track), Manifest, Worker, ServiceWorker,
SharedWorker, XSLT, Json, ...`. This is enough for accurate type-specific rules
(`$script`, `$image`, `$third-party`, `$subdocument`, etc.).

**To block a request**, the delegate calls:

```rust
load.intercept(WebResourceResponse::new(url)).cancel();   // → NetworkError::LoadCancelled
```

To do nothing, return without touching `load` (the responder defaults to
`DoNotIntercept` on drop — `webview_delegate.rs:256`). To *replace* (e.g. swap a
blocked tracker for a stub script, or return an empty 200), call `.intercept(resp)`
then `send_body_data(...)` + `finish()`.

### 1.2 Userscript & cosmetic-CSS injection — `UserContentManager`

`components/servo/user_content_manager.rs` + `components/shared/embedder/user_contents.rs`:

```rust
let ucm = Rc::new(UserContentManager::new(&servo));
ucm.add_script(Rc::new(UserScript::new(js_source, None)));         // JS into the page
ucm.add_stylesheet(Rc::new(UserStyleSheet::new(css_source, url))); // author/user CSS
// attach at build time:
WebViewBuilder::new(&servo, ctx).user_content_manager(ucm.clone())...build();
```

**Injection timing (verified):** `components/script/dom/userscripts.rs::load_script`
is called from `HTMLHeadElement` post-connect (`htmlheadelement.rs:61`) and queues a
*delayed task* that runs each script via `evaluate_js_on_global` in the page's main
realm. So userscripts run **early, around document-start, in the page's main world**
(no isolated world). Stylesheets are installed as user-origin stylesheets in the
style system (cascade below author styles unless `!important`).

### 1.3 Adjacent embedder surfaces we get "for free"

- `SiteDataManager` (`components/servo/site_data_manager.rs`): enumerate/clear
  cookies, localStorage, sessionStorage per-site — useful for a "clear data for this
  site" add-on action and for cookie-banner/consent handling.
- `NetworkManager`: cache inspection/clear.
- Per-rev (Servo 0.0.5+) embedding API now exposes HTTP proxy config, system root
  certs, and console messages — handy for privacy add-ons later.

### 1.4 Hard limits of the native surface (what is NOT there)

| capability | status at `ed1af70` | consequence for design |
| --- | --- | --- |
| `UserScript` match patterns | **none** — `UserScript` is just `{script, source_file}` | swerve must filter by URL itself and rebuild the UCM script set per navigation |
| Userscript injection time control (`document_start`/`idle`) | **none** — always the head delayed-task | "document-end" semantics must be emulated in-script (`DOMContentLoaded`) |
| Isolated world / content-script sandbox | **none** — runs in page main world | userscripts can be detected/clobbered by the page; no privilege boundary |
| `GM_*` / `chrome.*` / `browser.*` APIs | **none** | swerve must implement any privileged API itself |
| Background/event pages, service-worker extensions | **none** | no place to run an extension's persistent logic |
| Extension packaging (CRX/XPI/`manifest.json`) | **none** | no loader, no signing, no update protocol |
| `declarativeNetRequest` / `webRequest` extension APIs | **none** | content blocking must be embedder-native (which we do) |
| Cosmetic filtering primitives (`:has-text`, procedural) | **none** in Servo | adblock-rust scriptlets emulate these in JS |
| UCM live update without reload | **no** — doc comment says updates "take effect only after the page is reloaded" | filter-list changes apply on next navigation; acceptable |

---

## 2. The external pieces

### 2.1 adblock-rust (`adblock` crate)

- **Version 0.12.5** (current). Same engine shipping in Brave. Pure Rust, compiles
  native or WASM. **Not** currently in Servo's dependency tree, so adding it is a
  clean addition with no version-pin conflict.
- Capabilities: network blocking, **cosmetic filtering** (CSS hiding +
  scriptlets/`+js`), resource replacement/redirect, hosts-file syntax, uBlock Origin
  syntax extensions, and iOS content-blocking-list conversion.
- Core API shape (`adblock::Engine`):
  - `FilterSet::add_filters(rules, ParseOptions)` → `Engine::from_filter_set(set, optimize)`.
  - `engine.check_network_request(&Request)` → `BlockerResult { matched, redirect,
    important, ... }`. `Request::new(url, source_url, request_type)` where
    `request_type` is the string we derive from Servo's `Destination`.
  - `engine.url_cosmetic_resources(url)` → `UrlSpecificResources { hide_selectors,
    style_selectors, exceptions, injected_script, ... }` — the CSS selectors to hide
    and the JS scriptlets to run for a given page URL.
  - `engine.hidden_class_id_selectors(classes, ids, exceptions)` for class/id-based
    generic cosmetic rules (used incrementally as the DOM mutates — see §4.3 for why
    we mostly use the URL-specific path on Servo).
  - Serialization: a compiled engine can be cached to a blob (`engine.serialize`) for
    fast startup instead of re-parsing megabytes of lists each launch.
- License: MPL-2.0 — compatible with swerve (also MPL-2.0).

### 2.2 Filter lists (data, not code)

Ship a curated default set, updatable out-of-band (the "Lyku" sync service can host
mirrors / a manifest later):

| list | purpose |
| --- | --- |
| EasyList | base ad blocking |
| EasyPrivacy | tracker blocking |
| uBlock Origin filters (unbreak, badware, quick-fixes) | quality / site-fixups |
| Peter Lowe's / hosts-format list | DNS-style blocklist |
| Regional lists (opt-in) | localized ads |
| Annoyances / cookie-notice lists (opt-in) | Opera-GX-style "block cookie dialogs" |

These are licensed for redistribution but check each list's license; prefer fetching
from upstream with a bundled fallback snapshot so first-run works offline.

### 2.3 WebExtensions ecosystem reality check

- Servo: **no** WebExtensions support, none on the near-term roadmap.
- *Moto* (the only Servo browser explicitly targeting WebExtensions) is a
  side-project by one person and "has not been tracking the latest upstream changes
  to Servoshell for quite some time."
- A WebExtensions runtime is, in effect, a second browser inside the browser:
  hundreds of `chrome.*` namespaces, an extension process/permission model, CRX
  parsing + signature verification, an update protocol, an isolated-world content
  script injector, and (for MV3) a `declarativeNetRequest` rule engine. Chromium and
  Firefox each maintain this with large teams. **For swerve, full or even broad
  parity is out of scope for the foreseeable future.**

---

## 3. Recommended architecture

```
                        ┌──────────────────────────────────────────────┐
                        │                 swerve process                │
   filter lists  ───►   │  ┌────────────────────┐                       │
   (EasyList etc.)      │  │  Blocking Engine    │  adblock::Engine      │
   userscripts   ───►   │  │  + Userscript store │  (Arc, behind RwLock) │
   (.user.js)           │  └─────────┬──────────┘                        │
                        │            │ sync, in-memory match (<~50µs)    │
   ┌──────────────┐     │  ┌─────────▼──────────────────────────────┐   │
   │ HTML chrome  │◄────┼──┤ WebViewDelegate::load_web_resource      │   │ net thread
   │ (settings UI)│     │  │   → block / allow / redirect            │◄──┼── awaits reply
   └──────────────┘     │  └─────────────────────────────────────────┘   │
        ▲   swerve:     │  ┌─────────────────────────────────────────┐   │
        │   bridge      │  │ per-nav: rebuild UserContentManager:     │   │
        └───────────────┼──┤  • cosmetic CSS (url_cosmetic_resources) │   │
                        │  │  • scriptlets (+js)                      │   │
                        │  │  • matching userscripts                  │   │
                        │  └─────────────────────────────────────────┘   │
                        └──────────────────────────────────────────────┘
```

### 3.1 Network blocking path (the hot path)

On each `load_web_resource(webview, load)`:

1. If `load.request.is_for_main_frame` → never block (allow the document).
2. Build `adblock::Request` from `url`, `referrer_url` (source/first-party),
   and `destination`→type mapping. **First-party derivation:** prefer
   `referrer_url`; for `Document`/`Frame` subframes track the tab's current top URL
   in `AppState` so we always have the correct first party even when the referrer is
   stripped.
3. `engine.check_network_request(&req)`:
   - `result.matched && !result.exception` → `load.intercept(resp).cancel()` (block),
     or for some types return an empty 200 / a redirect stub (`result.redirect`) to
     avoid page breakage.
   - else → return (DoNotIntercept; load proceeds).
4. Bump a per-tab blocked-count for the toolbar badge (push to chrome via the
   existing `swerve:state` event).

### 3.2 Cosmetic + scriptlet path (per navigation)

`UserContentManager` updates only apply on the next page load, which lines up
perfectly with cosmetic filtering being per-URL:

- In `request_navigation`/`notify_url_changed` for a content tab, compute
  `engine.url_cosmetic_resources(url)`.
- Compose a single user stylesheet: `selectors { display:none !important }` for
  `hide_selectors`, plus any `style_selectors` (`:style(...)`) rules.
- Compose a single bootstrap userscript that (a) runs adblock-rust's injected
  scriptlets (`injected_script`) and (b) installs a small `MutationObserver` to apply
  procedural/`:has-text`-style rules adblock-rust can't express as plain CSS.
- Swap these into the tab's UCM and they take effect on the load. Because UCM updates
  need a reload to apply, build the cosmetic set **before** issuing the load where
  possible (intercept in `request_navigation`, set UCM, then allow).

> Note: each tab gets its **own** `UserContentManager` (cosmetic content is
> per-page), but the heavy `adblock::Engine` is a single shared `Arc<RwLock<Engine>>`.

### 3.3 Userscripts (swerve-native, the headline add-on type)

- A userscript store: `~/.config/swerve/userscripts/*.user.js`, each parsed for its
  Greasemonkey `==UserScript==` metadata block (`@match`/`@include`/`@exclude`,
  `@run-at`, `@grant`, `@name`, `@version`).
- On navigation, swerve evaluates `@match`/`@include` against the URL itself (Servo
  has no match support) and injects matching scripts via the tab UCM.
- Provide a minimal **`GM_*` / `swerve.*` shim** prepended to each script for the
  common grants: `GM_addStyle`, `GM_setValue`/`GM_getValue` (backed by a swerve
  key-value store synced via Lyku), `GM_xmlhttpRequest` (proxied through a privileged
  `swerve:` bridge call so it can bypass page CORS — this is the one capability that
  genuinely needs embedder cooperation), `GM_openInTab`, `GM_notification`.
- `@run-at document-end/idle` is emulated in the shim (wrap in a `DOMContentLoaded`/
  `requestIdleCallback`), since Servo only injects at head time.

### 3.4 The swerve-native add-on model (beyond userscripts)

A swerve "add-on" is a small signed bundle:

```
my-addon/
  addon.toml         # id, name, version, permissions[], entry points
  block/*.txt        # extra filter lists (declarative)
  scripts/*.js       # userscripts (with @match)
  styles/*.css       # user stylesheets (with match globs)
  panel/index.html   # optional chrome-side UI panel (rendered like the chrome)
```

- **Declarative-first.** Most useful add-ons (block lists, site CSS tweaks,
  redirects, header rewrites) are pure data + the engines swerve already runs. This
  avoids running arbitrary privileged code and dodges the WebExtensions process-model
  problem entirely.
- **Chrome-side panels.** Because swerve's chrome is itself HTML rendered by Servo,
  an add-on can contribute a panel/popup that the chrome composites — reusing the
  existing `swerve:` command bridge rather than inventing a new UI runtime.
- **A tiny `swerve.*` API** (not `chrome.*`) exposed to add-on contexts:
  `swerve.tabs`, `swerve.storage` (Lyku-synced), `swerve.contentBlocker` (toggle
  lists), `swerve.userScripts`, `swerve.notifications`. Small, owned by us, versioned
  by us — the opposite of chasing the WebExtensions surface.

---

## 4. Performance — the part that will bite

This is the single biggest engineering risk in the dimension, and it is structural to
how Servo surfaces interception.

### 4.1 Every request round-trips to the UI thread

`load_web_resource` is dispatched from `Servo::spin_event_loop` — i.e. on **swerve's
main/UI thread**, the same thread that composites and routes input. The net thread's
`main_fetch` **`.await`s** the reply for *every* request
(`fetch/methods.rs:537`), and the interceptor is taken behind a single
`TokioMutex` (`request_interceptor` is one shared lock). Implications:

- The block decision **must be near-instant and fully synchronous** inside
  `load_web_resource`. No disk I/O, no network, no list parsing on this path.
  `adblock::Engine::check_network_request` is an in-memory match (microseconds), which
  is fine — but we must compile lists *ahead of time* (load a serialized engine blob;
  never parse lists on the hot path).
- Any slow work on the UI thread (a janky chrome repaint, a long JS eval) delays
  *all* in-flight fetches, because they are blocked awaiting our reply. Keep
  `load_web_resource` doing nothing but a hashmap/trie lookup + a channel send.
- Because the interceptor holds one mutex across the await, requests are effectively
  serialized through the embedder. For a page with hundreds of subresources this is a
  measurable cost. **Mitigation:** answer immediately (no awaiting anything ourselves)
  so the lock is held only for the match. If this ever becomes a bottleneck, the
  fix is upstream (let net answer some requests without a round-trip) — track it, do
  not fork.

### 4.2 Engine memory & startup

- Full EasyList + EasyPrivacy + uBO lists compile to roughly tens of MB of engine
  state. Acceptable for a desktop browser, but: **serialize the compiled engine** and
  memory-map / load the blob at startup; only recompile when lists update (background
  thread, then atomically swap the `Arc<RwLock<Engine>>`).
- List updates: download on a timer / via Lyku, recompile off-thread, hot-swap. New
  rules apply to subsequent navigations.

### 4.3 Cosmetic filtering cost

- Prefer the **URL-specific** cosmetic path (`url_cosmetic_resources`) computed once
  per navigation over the incremental `hidden_class_id_selectors` path (which Chromium
  feeds with live DOM mutations). Servo gives us no cheap streaming class/id feed, and
  per-mutation IPC would be expensive — so do a single up-front injection plus an
  in-page `MutationObserver` for the long tail. This trades a little completeness for
  a lot of simplicity and performance.
- One combined user stylesheet + one combined bootstrap script per page keeps UCM
  churn low.

---

## 5. Security & permission model

Because there is **no isolated world** in Servo today, any injected userscript runs in
the page's main JS context. That shapes the trust model:

| add-on type | trust level | sandbox | permission gate |
| --- | --- | --- | --- |
| Built-in content blocker | swerve-trusted (ships with browser) | n/a (Rust, embedder side) | always on, user-configurable lists |
| Declarative add-on (lists/CSS/redirects) | low-risk (data only) | no code execution | install-time list of affected domains |
| Userscript | **high-risk** (arbitrary JS in page world) | none — runs as the page | per-script `@match` shown at install; `@grant`ed `GM_*` powers shown explicitly |
| `swerve.*`-API add-on | privileged | chrome-side panel is its own webview | explicit permission prompt per capability |

Principles:

1. **Permissions are explicit and per-capability**, surfaced at install and revocable
   in settings. A userscript that wants `GM_xmlhttpRequest` (cross-origin) or
   `swerve.tabs` must declare it; swerve shows the grant list before enabling.
2. **`GM_xmlhttpRequest` / cross-origin fetch is the one real privilege** — route it
   through a `swerve:` bridge call validated against the script's declared
   `@connect` domains, never by handing the page `fetch` to all origins.
3. **No silent auto-update of code.** Userscript/add-on updates that change code or
   add permissions require re-confirmation. (Filter *lists* are data and can
   auto-update.)
4. **Signing for the add-on store later.** v1 sideloads from disk with a clear "this
   runs code on the pages you visit" warning. A signed registry + Lyku-hosted
   distribution comes later.
5. **Content blocker integrity:** filter lists are data; the risk is a malicious list
   doing a `redirect` to a swerve-controlled resource. Only honor `redirect` rules
   that resolve to adblock-rust's bundled resource set, never to arbitrary URLs.
6. **Caveat the user honestly:** until Servo gains isolated worlds, swerve userscripts
   are *not* sandboxed from the page and a hostile page can interfere with them.
   Document this; do not imply WebExtensions-grade isolation.

---

## 6. Interaction with chrome & content webviews

- **Content blocker ↔ content webviews:** purely embedder-side. `load_web_resource`
  is per-`WebView`, so blocking is naturally per-tab; the per-tab `UserContentManager`
  carries that tab's cosmetic set. The blocked-request counter is pushed to the chrome
  via the existing `swerve:state` `CustomEvent` and rendered as a shield/badge.
- **Settings & controls in the chrome:** the chrome already drives the engine through
  `swerve:` command URLs intercepted in `request_navigation`. Extend that vocabulary:
  `swerve:blocker?toggle=easylist`, `swerve:blocker?allowlist=<host>`,
  `swerve:userscript?enable=<id>`. A `swerve://settings/extensions` chrome page lists
  installed add-ons, toggles, and per-site allowlists.
- **Add-on chrome panels:** an add-on's optional `panel/index.html` is composited the
  same way the chrome is (it *is* chrome), so no new rendering path. It talks to the
  engine over a scoped `swerve.*` API rather than raw `swerve:` navigation.
- **IPC control socket (M5):** extend the text protocol with
  `blocker on|off`, `blocker-stats`, `userscript-add <path>` so external tooling
  (and Lyku sync) can drive add-on state — consistent with the existing `SWERVE_IPC`
  design.

---

## 7. Phased plan

### Phase A — Content blocking MVP (highest value, lowest risk) — *do this first*
- Add `adblock = "0.12.5"` dependency.
- Implement `WebViewDelegate::load_web_resource`: derive `adblock::Request` from
  `WebResourceRequest`, call `check_network_request`, block via `intercept().cancel()`.
- Bundle EasyList + EasyPrivacy as fallback; compile to a serialized engine blob at
  build/first-run; load the blob at startup.
- Per-tab blocked counter → chrome shield badge.
- **Outcome:** a real, measurable ad/tracker blocker built in. Headline feature.
- *Est. effort: ~1–2 weeks. Risk: low (no Servo fork; one well-maintained crate).*

### Phase B — Cosmetic filtering + scriptlets
- Per-tab `UserContentManager`; per-navigation `url_cosmetic_resources` → combined
  user stylesheet + bootstrap script (scriptlets + `MutationObserver`).
- Add annoyance/cookie-notice lists (opt-in) for the "Opera-GX kills popups" feel.
- *Est. effort: ~2–3 weeks. Risk: medium (timing/`!important` cascade edge cases,
  per-nav UCM rebuild plumbing).*

### Phase C — Userscripts (the most-wanted "add-on")
- `.user.js` store, metadata parsing, URL `@match` filtering in swerve, UCM injection.
- `GM_*` shim incl. bridged `GM_xmlhttpRequest`; `@run-at` emulation; Lyku-backed
  `GM_setValue`.
- `swerve://settings/extensions` management page + per-script permission display.
- *Est. effort: ~3–4 weeks. Risk: medium (no isolated world; honest about it).*

### Phase D — swerve-native declarative add-ons + minimal `swerve.*` API
- `addon.toml` bundles (lists + scripts + styles + optional chrome panel).
- Small versioned `swerve.tabs/storage/contentBlocker/userScripts/notifications` API.
- Sideload-from-disk with permission prompts; signing/registry later.
- *Est. effort: ~4–8 weeks. Risk: medium; this is the durable extensibility story.*

### Phase E (optional, far horizon) — WebExtensions compatibility shim
- Only if there is real demand and Servo gains isolated worlds. Even then, target a
  *narrow* subset: load an unpacked MV3 dir, support `manifest.json` content_scripts
  + a `declarativeNetRequest`→adblock-rust translation + a handful of `chrome.*`
  read-only APIs. Background service workers, `webRequest`, and the full surface stay
  out of scope.
- *Est. effort: multi-person-quarter to -year. Risk: HIGH — this is the Verso
  treadmill. Gate it behind explicit demand and upstream isolated-world support.*

---

## 8. Open questions

- Does swerve track the per-tab top-frame URL well enough to feed adblock-rust the
  correct first-party for subframe requests? (Needs a small addition to `AppState`.)
- How aggressively should default blocking be on (Brave-style on-by-default vs. a
  setup choice)? Affects breakage support burden.
- Lyku as the distribution channel for lists/userscripts/add-ons: data format,
  signing, update cadence — coordinate with the sync dimension.
- Acceptable per-request overhead budget on the UI thread under a heavy page (target:
  match in single-digit microseconds; verify with a flamegraph on a real site).
- Whether to upstream an isolated-world / content-script-world primitive to Servo
  (benefits the whole ecosystem, but is itself sizeable) vs. living without it.
