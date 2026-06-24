# NavGator: Userscript system — concrete design

*Design doc. Grounds the userscript + permission + badge/popover work, and deliberately
shapes it as a **stepping stone** toward a future full WebExtensions runtime — not a parallel
dead-end. Verified against the repo (`/raid/NavGator`, `dev` branch). June 2026.*

Companion to [`extensions.md`](./extensions.md) (the strategic survey) and
[`engine-gap.md`](./engine-gap.md) (why Servo can't run real extensions today). This doc is
the *buildable* spec for the near-term piece.

---

## Implementation status (June 2026)

Built across `crates/navgator/src/userscripts.rs` (new, pure, **test-verified 34/34**) and
`crates/navgator/src/main.rs` (integration, **not compile-verified** — the workspace can't
build here, see [[navgator-build-env]]; needs a CI compile):

- ✅ Unified `Addon` registry + `Permission` enum + `AddonKind`/`AddonSource` (forward-compat).
- ✅ GM metadata parser, `MatchPattern` (`@match`/`@include`/`@exclude`), permission mapping.
- ✅ Per-add-on registry persisted as **`addons.json`** (serde_json — matches `PasswordStore`;
  `toml` deliberately not added).
- ✅ Install-consent dialog (reuses the `Dialog::Permission` pattern) + re-consent when a
  script's requested permissions grow.
- ✅ Per-site `@match` injection (per-tab UCM, scripts selected per navigation).
- ✅ `gator://extensions` manager page + enable/disable/remove command links.
- ✅ 🧩 toolbar badge (painted count bubble) + registry-driven popover.
- ✅ GM bridge: **unforgeable per-process-secret cap token**; `storage.{list,set,get,delete}`
  functional; `@connect` host allow-list enforced for `net.fetch`. Args travel in the URL
  query (Servo exposes no request body).

**Known limitations (engine-constrained, tracked):**
1. **First-load timing.** Servo applies UCM scripts on the *next* load and the tab isn't in the
   pane list during `build()`, so a script first applies on a reload of a matched page;
   `@run-at document-start` can't beat the first paint. Needs a UCM pre-seed-at-build or an
   engine UCM-swap primitive.
2. **Accumulation.** No UCM clear/remove primitive, so scripts attached for site A stay in a
   tab's UCM across later navigations (they're only *added* when matched, but not removed). A
   verified in-JS `location` guard inside `wrap_userscript` is the fix; deferred (subtle to get
   right unverified).
3. **Bridge reachability unverified.** Assumes Servo routes a page `fetch()` to the
   `navgator://` scheme into the embedder intercept (as it does for `gator://` page loads); not
   confirmed against a running build. The capability gate holds regardless.
4. **`net.fetch`/`notify.show`/`tabs.open`/`clipboard.set`** are permission- and `@connect`-gated
   but the actions aren't performed yet (return `*-unimplemented`); `net.fetch` needs an async
   HTTP client off the UI thread.
5. Injection is focused-pane-only in split mode (matches the existing delegate convention).

---

## 0. TL;DR

- Upgrade the naive userscript loader (`load_userscripts()`, `main.rs:673` — loads every
  `*.js`, injects on **all** pages, no metadata, no permissions) into a **metadata-aware,
  per-site, permission-gated** add-on system.
- Introduce **one unified `Addon` registry + `Permission` enum now**, even though only the
  `Userscript` kind is implemented. A future `WebExtension` kind slots into the *same*
  registry, *same* consent UI, *same* per-tab injection path, *same* badge/popover. The
  userscript work becomes ~70% of the plumbing a real extension runtime needs.
- Add a kind-agnostic **toolbar badge + popover** driven by the registry (the chrome already
  has the `egui::Area`+`Frame::popup()` pattern and an `adblock_blocked` counter to model it
  on).
- Be honest in the design about the ceiling: without Servo isolated worlds, userscripts are
  **not sandboxed from the page**, and "bring your own Chrome Web Store extension" is *not*
  delivered by this work — see §9 for exactly what we win and what we don't.

---

## 1. Current state (verified)

| piece | where | state |
|---|---|---|
| Userscript loader | `load_userscripts()` `main.rs:673` | reads all `*.js` from `userscripts_dir()` (`main.rs:666`), sorts, returns `(path, src)` |
| Shared injection | `BrowserState.userscripts: Option<Rc<UserContentManager>>` `main.rs:1403` | one UCM, Rc-cloned onto **every** tab/popup webview → every script runs on every page |
| Adblock engine | `BrowserState.adblock` + `adblock_blocked: Cell<u64>` `main.rs:1415` | adblock-rust wired; network block in `load_web_resource`; session counter |
| Cosmetic filter | `pending_cosmetic` `main.rs:1419`, `COSMETIC_COLLECT_JS` `main.rs:4516` | deferred **JS-eval** injection: collect class/id set, inject a `<style>` |
| Web-permission gate | `request_permission()` `main.rs:4966` → `Dialog::Permission{message,handle}` `main.rs:3735` | Servo Geolocation/Notifications → egui Allow/Deny window |
| Internal pages | `gator://` scheme via `AppState::load_web_resource`; `gator://settings?key=value` parsed `main.rs:1082` | welcome/crash/downloads/passwords/settings all server-rendered |
| Built-in password store | `password.rs` (`PasswordStore`: `for_origin`, `upsert`, E2EE, keyring unlock) | real autofill via `READ_FORM_JS` eval; result channel deliberately never touches page-readable storage (`main.rs:4400`) |
| Script→Rust channel | `tab.evaluate_javascript(js, callback)` `main.rs:4334` | callback receives the eval result — the substrate for a bridge *response* |

Two facts shape the whole design:
1. **There is already a deferred JS-eval injection path** (cosmetic filter) *and* the UCM
   path. We get to choose per use-case.
2. **NavGator already ships a capable E2EE password manager.** This changes the
   "we'll lose password-manager users" calculus (§9).

---

## 2. Data model — the unified add-on registry (forward-compat core)

Define this now. It is the single most important forward-compatibility decision: the consent
UI, persistence, badge/popover, and settings page are all written against `Addon`/`Permission`,
so a `WebExtension` kind later is *additive*, not a rewrite.

```rust
/// Stable identity: for userscripts, a hash of the @name+@namespace (falls back to file path).
struct AddonId(String);

enum AddonKind {
    Userscript,
    Declarative,        // future: pure block-list / CSS / redirect bundles (extensions.md §3.4)
    WebExtension,       // future: unpacked MV3 dir (extensions.md Phase E) — gated on isolated worlds
}

enum AddonSource {
    Userscript { path: PathBuf, content_hash: u64 },
    // WebExtension { dir: PathBuf, manifest_hash: u64 },  // later
}

struct Addon {
    id: AddonId,
    kind: AddonKind,
    name: String,
    version: String,
    author: Option<String>,
    description: Option<String>,
    enabled: bool,
    matches: Vec<MatchPattern>,    // @match / @include  (host access == a permission, see §3)
    excludes: Vec<MatchPattern>,   // @exclude
    run_at: RunAt,                 // DocumentStart | DocumentEnd | DocumentIdle
    requested: PermissionSet,      // what the script's metadata asked for
    granted:   PermissionSet,      // what the user approved (subset of requested)
    connect: Vec<Host>,            // @connect allow-list for cross-origin fetch
    source: AddonSource,
}
```

### The Permission enum — union of "userscripts need now" + "extensions need later"

```rust
enum Permission {
    /// Host access. The CORE permission. @match globs == MV3 host_permissions.
    RunOnSite(MatchPattern),
    /// GM_xmlhttpRequest / cross-origin fetch, scoped to @connect hosts. The ONE
    /// capability that genuinely needs embedder cooperation (bypasses page CORS).
    CrossOriginFetch,
    /// GM_setValue/getValue — per-addon key-value store (Lyku-syncable later).
    Storage,
    /// GM_notification.
    Notifications,
    /// GM_openInTab / future navgator.tabs.
    TabControl,
    Clipboard,
}
```

MV3 manifests map cleanly onto this later: `host_permissions`→`RunOnSite`,
`"storage"`→`Storage`, `"notifications"`→`Notifications`, `"tabs"`→`TabControl`, host-scoped
`fetch`→`CrossOriginFetch`. Same enum, same consent dialog, same revocation UI.

**Persistence:** a single `addons.json` under the config dir (sibling to `passwords.enc`),
holding the registry + per-addon `enabled`/`granted`/`content_hash`. The `*.user.js` files stay
on disk as the source of truth for code; `addons.json` holds *state and consent*.

---

## 3. Userscript lifecycle

```
discover → parse metadata → diff vs registry → (consent if new/changed) → persist → inject
```

1. **Discover.** Scan `userscripts_dir()` for `*.user.js` (and a managed `installed/` subdir
   for store-installed scripts later). Keep the legacy bare `*.js` support as "trusted,
   no-metadata, all-sites" for back-compat, but steer new installs to `*.user.js`.
2. **Parse** the Greasemonkey `// ==UserScript== … // ==/UserScript==` block:
   `@name @namespace @version @description @author @match @include @exclude @run-at
   @grant @connect`. Small hand-rolled parser; no new heavy dep.
3. **Diff** against the registry by `content_hash`:
   - new id → install consent prompt.
   - same id, hash changed → re-prompt **only if code changed or the requested permission set
     grew** (the "no silent auto-update of code" principle, `extensions.md §5`). A pure
     metadata-identical reload re-injects silently.
4. **Consent** (§6): map `@match`/`@grant`/`@connect` → `requested: PermissionSet`, show the
   dialog, store `granted` and `enabled=true` on Allow.
5. **Persist** to `addons.json`.

`@grant` → permission mapping:

| metadata | `Permission` | needs embedder? |
|---|---|---|
| `@match`/`@include` | `RunOnSite(glob)` | yes (per-tab injection filter) |
| `@grant GM_xmlhttpRequest` + `@connect` | `CrossOriginFetch` | **yes — the real one** |
| `@grant GM_setValue/GM_getValue/GM_deleteValue` | `Storage` | yes (KV store) |
| `@grant GM_notification` | `Notifications` | yes |
| `@grant GM_openInTab` | `TabControl` | yes |
| `@grant GM_addStyle` | (none — pure in-page CSS) | no |
| `@grant none` | only `RunOnSite` | no |

---

## 4. Injection architecture — the engine-facing core

The central change: the single shared UCM (`main.rs:1403`) cannot honor `@match`. We need
**per-site script selection**. Two viable mechanisms, both already proven in the tree:

### Option A — per-tab UserContentManager, rebuilt on navigation (recommended)
On each top-frame navigation of a content tab (`request_navigation` / URL-changed), compute the
enabled scripts whose `matches` accept the URL and `excludes` don't, build a `UserContentManager`
containing those (each wrapped per §5), and attach it to the tab's webview. Cache UCMs keyed by
the *set of matched script ids* so repeat navigations to the same site reuse one.

- **Pro:** scripts run at **document-start in the head** (Servo's UCM injection point) — correct
  `@run-at document-start` semantics, which the eval path can't give.
- **Con:** UCM updates apply on *next* load, so compute the set **before** allowing the load
  (intercept in `request_navigation`, set UCM, then proceed — `extensions.md §3.2` already notes
  this ordering).

### Option B — deferred JS-eval injection (reuse the cosmetic path)
Inject matched scripts via `evaluate_javascript`, exactly as the cosmetic filter injects its
`<style>` (`main.rs:4516`), URL-filtered in Rust.

- **Pro:** zero new engine plumbing; reuses a working deferral mechanism.
- **Con:** runs *after* the eval fires (≈ document-end at best) — wrong for `@run-at
  document-start` scripts that must beat page scripts.

**Decision:** use **A** for injection (timing matters for the headline userscripts), fall back
to **B**'s deferral discipline for any post-load work. `@run-at document-end/idle` is emulated
in the shim regardless (wrap body in `DOMContentLoaded` / `requestIdleCallback`), since UCM only
gives head-time injection.

> Each tab gets its own UCM (per-site script set); the registry + parsed scripts are shared on
> `BrowserState`. Mirrors how the adblock `Engine` is shared but cosmetic content is per-page.

---

## 5. The `GM_*` capability bridge + security model

Each script's source is wrapped at injection time:

```js
(function () {
  // 1. Capture pristine references BEFORE page script runs (document-start UCM injection),
  //    so a hostile page can't shadow fetch/XHR to intercept bridge traffic.
  const __nativeFetch = fetch, __XHR = XMLHttpRequest;
  // 2. Per-injection capability token, bound to this addon id + granted perms in Rust.
  const __cap = "<opaque-token>";
  const GM_setValue = (k,v) => __bridge("storage.set", {k,v});
  const GM_xmlhttpRequest = (o) => __bridge("net.fetch", o);   // gated by CrossOriginFetch + @connect
  // … other granted GM_* …
  function __bridge(call, args) { /* see below */ }
  // 3. @run-at emulation
  const __run = () => { /* original script source */ };
  // run-at: document-start → now; document-end → DOMContentLoaded; idle → requestIdleCallback
})();
```

**Request channel (script → Rust):** the shim issues `__nativeFetch("navgator://gm/<cap>/<call>", …)`.
This is intercepted in **`load_web_resource`** — the *same* hook adblock already uses
(`main.rs`) — which is naturally per-webview. Rust validates `<cap>` against the calling addon's
`granted` set and (for `net.fetch`) the request host against `@connect`, performs the privileged
action, and returns the result as the response body. The grant check is the enforcement point.

**Response channel (Rust → script):** the `navgator://gm/…` fetch resolves with the result; for
fire-and-forget calls, no body. (We deliberately do **not** reuse `evaluate_javascript` for
responses to a specific script — there's no isolated world to target.)

### Security — stated honestly (Servo has no isolated world)

| threat | mitigation | residual risk |
|---|---|---|
| Page shadows `fetch`/`XHR` to steal bridge traffic | capture pristine refs at document-start, before page script | a page that itself injects at document-start *could* race; best-effort only |
| Page reads another script's capability token | token lives in a closure, not on `window`; per-addon | a determined hostile page sharing the main world can probe — **not** a hard boundary |
| Script attribution (which script made a call?) | per-injection `__cap` token bound server-side to addon id | tokens are bearer creds in a shared world; treat as soft attribution |
| `CrossOriginFetch` abused | host-scoped to `@connect`, shown at consent, validated in Rust | only as tight as the declared `@connect` |
| Malicious storage redirect (adblock `redirect` rules) | already handled: honor redirects only to bundled resources (`extensions.md §5.5`) | — |

**The honest line for users (and the UI):** userscript *capabilities* (cross-origin fetch,
storage, notifications, tab control) **are** gated and enforced in Rust, and **which sites a
script touches** is gated by `@match`. But because there is no isolated world, a userscript is
**not sandboxed from the page it runs on** — a hostile page can interfere with a script sharing
its context. This must be surfaced at install ("runs as code on these pages"), not buried. It is
the same caveat `extensions.md §5` commits to, made concrete.

---

## 6. Permission UI + settings

- **Install consent dialog:** reuse the `Dialog::Permission` pattern (`main.rs:3735`). Title the
  script, list `granted` candidates in human terms:
  > **Enable “GitHub Dark” v1.4?**
  > • Runs on: `github.com/*`, `gist.github.com/*`
  > • Can fetch from: `api.github.com` (cross-origin)
  > • Can store data
  > ⚠ Runs as code on those pages — it is not sandboxed from them.
  > [Enable] [Cancel]

  Allow → `granted = requested`, `enabled = true`, persist. (Later: per-permission checkboxes to
  grant a subset — the enum supports it.)
- **Management page:** `gator://settings/extensions` (or a section of `gator://settings`),
  rendered the same server-side way as `gator://passwords` (`main.rs:1656`). Lists each `Addon`:
  enable/disable toggle, granted permissions with revoke, `@match` list, version, "remove".
  Toggles ride the existing `gator://settings?key=value` link mechanism (`main.rs:1082`).

---

## 7. Toolbar badge + popover (kind-agnostic, registry-driven)

- **Badge button:** one button in the right-to-left toolbar group in `draw_chrome()`
  (`main.rs:2484–2527`), beside ☰/🎨 — a 🧩 glyph. Paint a count bubble over its response rect
  with `ui.painter().circle_filled()` + text (egui has no built-in badge; ~10 lines). Count =
  scripts active on the current tab (and/or blocked-request count, which `adblock_blocked`
  already provides).
- **Popover:** clicking toggles an `egui::Area::new(...).fixed_pos(rect.left_bottom())` +
  `egui::Frame::popup()` — the established pattern (omnibox suggestions `main.rs:2693`, context
  menu `main.rs:3763`). Contents are driven by the **registry**, so it's kind-agnostic: each
  `Addon` shows name + an "active on this page" dot + a quick enable toggle; footer links to
  `gator://settings/extensions`.
- **Per-add-on popups later (the `chrome.action` equivalent):** when an add-on wants its own UI,
  two paths from `extensions.md §3.4`: render a **native egui panel** from declarative metadata
  (v1), or composite a **small Servo WebView** inside the popover Area the way page content is
  composited (HTML popups; reuses the page-render path). The badge/popover surface is designed so
  a `WebExtension` action popup drops into the same place.

---

## 8. Forward-compatibility with a full WebExtensions runtime

This is the design's reason for being shaped the way it is. When (if) we build the Phase-E MV3
shim, these userscript pieces are **reused, not replaced**:

| userscript piece | what a WebExtension runtime reuses |
|---|---|
| `Addon` registry + `addons.json` | add `AddonKind::WebExtension` + `AddonSource::WebExtension{dir}` |
| `Permission` enum + consent dialog | MV3 `permissions`/`host_permissions` map onto the same enum + UI |
| Per-tab UCM injection (§4) | MV3 `content_scripts` are the same per-site injection, with match-globs we already parse |
| `navgator://gm/…` bridge (§5) | becomes the substrate for a minimal `chrome.*`/`browser.*` shim |
| Badge + popover (§7) | hosts the extension `action` badge + popup |
| adblock-rust engine (already present) | `declarativeNetRequest` rules translate into it |
| Built-in E2EE password store | the native answer that reduces dependence on extension password managers (§9) |

The two things the runtime still has to add that userscripts genuinely lack — and the reason
full extensions remain a separate, fundable workstream — are **(a) an isolated content-script
world** (an upstream Servo ask; until then no real sandbox) and **(b) background service
workers** (no place to run persistent extension logic today). Everything *else* is built by this
design. That's the point: don't pay for the registry/permissions/injection/UI twice.

---

## 9. The marketplace reality — what this wins, what it doesn't

The strategic worry is correct and worth stating bluntly: **a perf/privacy-conscious user who
can't get their favorite extensions will leave.** So, concretely, against the actual top of that
list:

| user's must-have | delivered by | verdict |
|---|---|---|
| **uBlock Origin** (ad/tracker block) | built-in adblock-rust engine (already shipping) | ✅ covered natively; arguably better-integrated |
| **Dark Reader** | ships a **userscript build** → runs on this system; could even be bundled built-in | ✅ deliverable via this work |
| **Tampermonkey/Greasemonkey scripts** | this *is* that | ✅ native |
| **Password manager (built-in vault)** | NavGator's own E2EE store (`password.rs`) with autofill | ✅ for the mainstream case — native, not an extension |
| **1Password / Bitwarden (their existing vault)** | needs: form autofill (userscript-doable) **+ background SW + native messaging + own popup** | ⚠ partial; full feature parity needs the runtime *and* native messaging |
| **React/Vue DevTools, Grammarly, complex MV3 apps** | need background SW / deep `chrome.*` / isolated world | ❌ require the full runtime |

So the honest framing: the userscript + built-ins approach covers a **surprisingly large slice**
of the "must-have" list — ad-blocking ✅, dark mode ✅, password management ✅ (native), arbitrary
site tweaks ✅. What it does **not** deliver is the specific expectation *"install my existing
extension from the Chrome Web Store unmodified."* That expectation is met **only** by the
WebExtensions runtime, which stays the big, fundable, later workstream gated on Servo isolated
worlds (`extensions.md` Phase E, `engine-gap.md §13`).

**Strategic conclusion:** ship the userscript system now (it's the durable extensibility
substrate *and* covers the common cases), market the built-ins honestly (block + dark + passwords
are first-class, not bolt-ons), and treat the WebExtensions runtime as the funded path to "bring
your own extension" — built *on top of* this design, not beside it. Do not promise Chrome Web
Store compatibility before isolated worlds land upstream.

---

## 10. Phased build plan

**P1 — Registry + metadata + consent (no per-site yet).** `Addon`/`Permission` types,
`addons.json`, GM metadata parser, install consent dialog (reuse `Dialog::Permission`),
`gator://settings/extensions` list with enable/disable + remove. Scripts still inject on all
matched sites via the existing shared UCM as a stopgap. *Small–moderate.*

**P2 — Per-site injection.** Per-tab UCM rebuilt on navigation (§4 Option A), `@match`/`@exclude`
filtering in Rust, `@run-at` emulation in the shim. *The core plumbing; moderate.*

**P3 — GM_* bridge.** `navgator://gm/…` interception in `load_web_resource`, `GM_addStyle`,
`GM_setValue/getValue` (per-addon KV store), `GM_notification`, `GM_openInTab`, and the gated
`GM_xmlhttpRequest` validated against `@connect`. Per-script capability token + pristine-ref
capture. *Moderate; the security-sensitive part.*

**P4 — Badge + popover.** Toolbar 🧩 button with count bubble; registry-driven popover; per-tab
active-script indicator. *Small.*

**P5 — Polish.** Subset-permission grants (per-cap checkboxes), Lyku-synced `GM_setValue`,
script auto-update with re-consent on code/permission change, import from Greasy Fork URLs.

---

## 11. Open questions

- **Script identity in a shared world.** The capability token is a bearer credential in the
  page's main world. Is soft attribution acceptable for v1, or do we wait on (and possibly
  upstream) a Servo isolated-world primitive before exposing `CrossOriginFetch`? (Leaning:
  ship with the documented caveat; `CrossOriginFetch` off-by-default per script.)
- **First-party / top-frame URL** for `@match` on subframes — same `AppState` addition the
  adblock first-party derivation needs (`extensions.md §8`); share it.
- **Bundled defaults?** Ship a curated Dark-Reader-equivalent userscript on by default, or keep
  the store empty and discoverable? (Affects the "it just works" first-run impression.)
- **Greasy Fork / OpenUserJS install flow** — intercept `*.user.js` navigations and route to the
  consent dialog instead of downloading, like Tampermonkey does.
- **Where does the WebExtensions runtime decision get made?** Track Servo isolated-world progress
  as the gating signal; revisit Phase E when it lands or when demand + funding justify upstreaming
  it ourselves.
