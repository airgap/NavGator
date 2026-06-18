# swerve master roadmap

Lead-architect synthesis of the ten dimension analyses in [`docs/plan/`](plan/).
Read this first; each section links the deep-dive it condenses. Written 2026-06-18
against Servo rev `ed1af70`, stable Rust 1.95, an ~848-package dep graph, and a
788-line single-binary prototype at M1–M5.

This document is deliberately unhyped: where a thing is multi-year or not feasible it
says so. **§R2 below (owner decisions, 2026-06-18) overrides the resourcing, scope,
platform, and engine-strategy assumptions in §0–§10.** Those sections were written for a
resource-constrained, upstream-first, Linux-first, "second browser" posture; the owner
has since chosen the opposite on every axis. Read §R2 first; treat §0–§10 as still-valid
*technical* analysis whose *strategic framing* is superseded.

---

## R2. Owner decisions — locked 2026-06-18 (authoritative; overrides §0–§10 framing)

Self-funded, with staffing resources, the owner chose the maximal-ambition path. The six
locked decisions and their honest consequences:

| # | Decision | Overrides | Consequence (straight) |
|---|----------|-----------|------------------------|
| D1 | **Fork Servo; implement everything ourselves; do NOT file upstream.** | §7 upstream-first; risk #1 framing | We become an **engine vendor**, not an embedder. The "keep-up-with-upstream treadmill" is replaced by **owning the whole web platform** (layout, SpiderMonkey/JS, net, media, security patches) — the single largest cost in the plan; it dwarfs the browser-UI work. Legitimate with real resources (it's what a browser *company* is), but eyes-open. **Sub-decision (D1a):** *hard fork* (permanent divergence; forgo all future Servo work) vs *maintained fork* (rebase on upstream Servo on a cadence + carry our patches). **Recommend maintained fork** — pure divergence rots and forfeits Servo/Igalia's ongoing engine output. "No upstreaming" is a fine *policy*; a periodic **merge-from-upstream cadence is still required** or the fork becomes unmaintainable. **→ Locked: maintained fork** — swerve pins to its own Servo fork, rebases/merges upstream on a scheduled cadence, patches on top. |
| D2 | **Linux, macOS, Windows all first-class from day one.** | §2 Linux-x86-64-first + the Phase-5 fallback | All three sandboxes (Linux seccomp+userns, macOS Seatbelt, Windows AppContainer + job objects) and tri-platform GPU/`surfman`/ANGLE bring-up are on the critical path from the start. ~**3× the platform/security/CI surface**; Servo's macOS/Windows sandboxing is weak/absent, so most of it we build in the fork (consistent with D1). |
| D3 | **No cryptocurrency/web3 wallet.** Keep the built-in password manager with **opt-in E2EE sync.** | adds an anti-bloat non-goal | Corrected reading: "86 the crypto wallet" = the **cryptocurrency wallet** (the Brave/Opera-style bloat) — an explicit non-goal, *not* credentials. The **zero-knowledge password vault stays** and syncs **opt-in** via the Lyku account (design: [`sync-lyku-integration.md`](plan/sync-lyku-integration.md)). The separate-sync-passphrase friction (Lyku is passwordless) therefore hits **only users who opt into password sync**; local-only users never see it. Risk #8 (crypto correctness, external review before the vault ships) stands. |
| D4 | **Self-funded.** | §2/§7a funding gate; "labeled-preview-only"; risk #2 | Funding gate **satisfied** — a public, safety-claiming 1.0 is no longer grant-blocked. Caveat: money ≠ instant expertise; the pool of *Servo-fork + SpiderMonkey + browser-internals* engineers is narrow, so **hiring becomes the critical-path constraint** and bus-factor persists until the team exists. |
| D5 | **Target full web rendering (Chrome-ish parity).** | §1/§2 "non-DRM mainstream web, not parity"; most §2 non-goals | The de-scoped engine-blocked features — passkeys/WebAuthn, state-partitioning, real downloads/find APIs, IndexedDB, WebGL2/WebGPU, service workers, WebRTC — all move **in-scope, built in our fork**. Reality check: Servo is ~62% WPT / ~20% Baseline-Widely-Available today; closing to Chrome parity is historically **thousands of engineer-years** (only Google/Apple/Mozilla have done it). With sustained resources it's a *multi-year company mission*, not a release. Still **sequence** it (most-used features first, measured vs a real top-sites corpus) rather than chase 100% WPT. **→ Locked: DRM/EME IN SCOPE.** Pursue a Widevine/PlayReady **CDM license** — a **business/legal track, not in-fork engineering**: we build the EME plumbing in the fork to *host* the CDM, but the CDM binary is proprietary (Google/Microsoft). It is the **one deliberate exception** to engine independence — gate it behind a build flag and document the dependency. |
| D6 | **Resources available (funding + staffing).** | the "~1-person project" framing | Re-frames the plan from solo-constrained to team-scaled. Phase *sequencing* holds; durations compress with headcount — but the **engine-ownership (D1) + parity (D5) + tri-platform (D2)** combination is the defining, dominating cost. |

**Net:** these turn swerve from "a themeable independent *second* browser" into **"build an independent full web platform + browser + browser company, on a Servo fork, across three OSes."** The most ambitious undertaking in consumer software — internally coherent *given real, sustained resources*. The dominating work item is now **the engine fork**; the gating constraint is now **hiring engine-capable engineers**, not money or upstream cooperation.

**Resolved 2026-06-18:** D1a → **maintained fork**; D3 → **no cryptocurrency wallet; keep the password manager with opt-in zero-knowledge Lyku sync**; D5a → **DRM/EME in scope via CDM licensing** (the one allowed proprietary dependency).

---

## 0. What changed in this revision (read first)

This final cut closes five gaps and tempers five overclaims that an adversarial reader
(or a funder doing diligence) would otherwise catch:

- **Compat number pinned to one source.** The single most-repeated metric was
  internally contradictory: ROADMAP/engine-gap said *87/439 (19.8%)* while positioning
  said *87/593 (19.8%)* — and 87/593 is 14.7%, not 19.8%. Verified against the source on
  2026-06-18: webtransitions.org reports a **593-feature BWA catalog**, but its headline
  percentages are computed over the **439 features that carry a recorded status**:
  **87 full = 19.8%, 333 partial = 75.9%, 19 unsupported = 4.3%.** Canonical statement
  everywhere now: **"87 of 439 categorized BWA features at production quality (19.8%);
  333 partial; 593 in the catalog."** 593 is the catalog total, never the percentage
  denominator. The FTE/€ and "~80% by 2037" figures are **one external linear
  projection 11 years out — directional, not authoritative**, and are flagged as such.
- **Sandbox re-phased as the gating, most-likely-to-slip deliverable** with a
  **pre-decided Linux-x86-64-only-v1 fallback** and a **"file the pluggable-sandbox
  upstream RFC now"** action on the critical path (§3 Phase 5, §8).
- **Two adoption-critical P0s added:** (a) **import from other browsers**
  (bookmarks/history/passwords/cookies) on first run; (b) a **no-telemetry-compatible
  field-quality signal** (opt-in crash reports + local error log + "report this site")
  that resolves the "verifiable zero telemetry vs how do we learn it's broken" tension.
- **Mobile/Android analyzed, not silently deferred** (§2a) — including whether the GX
  refugee persona is reachable desktop-only.
- **"An afternoon each" downgraded.** The delegate cluster is re-sequenced strictly
  *behind* the SQLite store + `swerve://` internal pages + a security-reviewed dialog
  track, and re-estimated honestly (§3 Phase 1).
- **Funding/headcount reconciled as a gate, not a risk-row** (§7a): which phases are
  deferred indefinitely if funding stays Tier-1, and "recruit one co-maintainer" made a
  dated precondition for Phase 5.
- **Tempered overclaims, now caveated in place:** passkeys are *definitively absent and
  multi-quarter engine-blocked* (verified: no `PublicKeyCredential`/authenticator
  WebIDL, `dom_credential_management_enabled=false`); the footprint story is the
  *prototype's*, not 1.0's; the "process-per-domain removes G1" framing now states the
  Spectre/OOPIF residual plainly; the crypto-is-"nearly-free" framing now leads with the
  RC-crate risk; downloads/find-in-page are *no-primitive, must-upstream* items that
  feed the #1 treadmill risk.
- **New planned dimensions** previously missing entirely: testing/QA & web-compat
  regression corpus (§3 Phase 0/7, §6a), state partitioning & anti-fingerprinting (§6b,
  verified engine-blocked), localization/i18n (§6c), legal/licensing/trademark (§6d),
  distribution/packaging mechanics (§6e), GPU/driver/surfman portability (§6f).

---

## 1. Executive summary + the independence thesis

swerve is a web browser whose **chrome (its own UI) is HTML rendered by Servo**, with
web pages composited alongside it via an `OffscreenRenderingContext`. M1–M5 built a
real browser *frame* — HTML chrome, multi-webview compositing, tabs, an
omnibox/back/forward navigation bridge, mouse+keyboard routing, and an opt-in
external IPC control socket. It does **not** yet have a single day-to-day product
feature (no bookmarks, history, downloads, settings, passwords, sessions) and has
**zero persistence** — `ServoBuilder::default()` is used with no `config_dir`, so even
Servo's own cookie jar is ephemeral.

**The independence thesis (true, but not a consumer wedge on its own).** Every viable
alternative browser traces to Google's engine (Blink, >75% of sessions) or Google's
money (Mozilla ~85% search-deal revenue, expiring end-2026). Only swerve and Ladybird
run on engines that are neither Blink nor Google-funded. That independence is real and
is swerve's credibility layer — but Firefox at 2.26% share proves values-alone barely
sells, and Servo's web compat (~62% WPT, **87/439 categorized Baseline-Widely-Available
features at production quality = 19.8%; 333 partial; 593 in the catalog**, with an
external projection of an ~80% plateau around 2037 that is directional, not
authoritative) means swerve **cannot win on parity**. The honest positioning is
**"second browser, by choice"**: win on deep, performant theming (the
HTML-chrome-in-Servo architecture is the one un-copyable feature), verifiable zero
telemetry, a built-in MV3-proof content blocker, lean footprint, and self-hostable
end-to-end sync. Independence is the foundation, not the headline. See
[`positioning.md`](plan/positioning.md).

**The dominant risk is not compat — it is the Servo-embedding maintenance treadmill**
that archived the direct predecessor **Verso (Oct 8 2025)**. swerve already paid the
architectural insurance Verso skipped (high-level `servo` umbrella crate, ~30 public
symbols, pinned rev, no self-owned compositor, an 788-LOC diff vs Verso's ~30 crates +
2,200-LOC compositor). The ground also shifted in swerve's favour in April–May 2026:
Servo published to crates.io and launched an **LTS train with half-yearly migration
cycles**, converting "chase a HEAD that breaks ~3–6×/month" into "one scoped migration
twice a year." The residual risk is therefore **process discipline and bus-factor (=1
today)**, not architecture. **Note that every "must-upstream" item below — downloads,
find-in-page, a pluggable sandbox — *re-introduces* this exact treadmill risk by making
swerve carry a patch across every LTS bump; the plan does not pretend otherwise.** See
[`sustainability.md`](plan/sustainability.md).

**Scale, honestly.** A usable daily-driver 1.0 is a **2–4 year, 1–6 person effort**,
gated by Servo's own web-platform ceiling. True Chrome parity is **never** — and the
README and product should say so.

---

## 2. "swerve 1.0" MVP definition + explicit non-goals

**swerve 1.0 is a usable, themeable, private second browser for the non-DRM
mainstream web, on Linux x86-64 first.** It is defined by these properties:

- **Imports the user's life from their old browser on first run** (P0, not P2):
  bookmarks, history, and — where the source format allows — saved passwords and
  cookies from Chrome/Chromium, Firefox, and Edge. A browser you cannot move *into* is a
  browser nobody adopts. First-run funnel: import → pick theme → set default-search →
  optional sync sign-in.
- **Renders and operates the non-DRM mainstream web** without a "broken browser" feel:
  the unimplemented-but-hook-exists delegate cluster is wired (context menus, JS
  dialogs, form/file/color pickers, permission prompts, HTTP auth, popups, favicons),
  `<video>`/`<audio>` actually decode (media backend ON), and the cheap silent-breaker
  DOM APIs are enabled and test-gated.
- **Persists everything** via a per-profile dir + SQLite "places" store, with Servo's
  `config_dir` set so cookies/localStorage survive: bookmarks, history, downloads,
  settings, sessions, permissions, zoom — each built with a sync-record envelope from
  day one.
- **Core product features** present: omnibox with frecency suggestions, bookmarks,
  searchable history, downloads + manager, find-in-page, session/crash restore,
  settings UI, per-site permissions, zoom, clipboard, full keyboard-shortcut set,
  `swerve://` internal pages.
- **Deep performant theming** as the headline: a `--sw-*` design-token catalog, chrome
  hot-reload (<16ms), live light/dark + force-dark for content, per-site themes,
  wallpapers via a `swerve-asset://` handler, a validated package format, and a
  **CI-enforced perf budget** (the GX differentiator is being fast, not feature count).
- **Built-in content blocking on by default** (adblock-rust + EasyList/EasyPrivacy via
  `load_web_resource`), plus userscripts as the native add-on type.
- **Sandboxed multiprocess content on by default — on Linux x86-64, the 1.0 security
  bar** (see §3 Phase 5; macOS/Windows sandboxes are explicitly post-1.0). Privileged
  chrome moved off `file://` onto an internal scheme with a typed JSON bridge, signed
  auto-update, and a local-hash-prefix Safe-Browsing-equivalent.
- **Self-hostable, opt-in, end-to-end-encrypted sync** (Lyku + self-host server +
  local-folder providers behind one `SyncProvider` trait), zero-knowledge by default.
- **Verifiable zero telemetry** *paired with* an **opt-in, transparent field-quality
  signal**: local-only error log, opt-in crash report (showing the exact payload before
  send), and a "report this site" button. The "what swerve sends" page documents both.
- **Localizable from day one** (string catalog, no hardcoded English in chrome) even if
  only English + 1–2 community locales ship at 1.0; RTL/IME remain engine-blocked
  stretch goals, stated honestly.

**Explicit non-goals for 1.0 (declared, not apologized for):**

- **No DRM / EME** — no Netflix, Disney+, Spotify-web, Prime. MediaKeys WebIDL is
  entirely absent from Servo and off its roadmap. Covered by an open-in-system-browser
  escape hatch.
- **No WebRTC calling** (Meet/Discord-web/Jitsi) — servo-media backend is immature.
- **No Chrome-extension (CRX/MV3) compatibility** and **no Chrome DevTools/CDP** — no
  Puppeteer/Playwright-chromium. swerve ships a native add-on model + WebDriver +
  Firefox-RDP instead.
- **No HTTP/3/QUIC** — H1+H2 only (functional, slower on Google/CDN traffic).
- **No passkeys/WebAuthn** — **definitively engine-blocked, not merely "unconfirmed":**
  Servo ships `Credential`/`CredentialsContainer`/`PasswordCredential` WebIDL but **no
  `PublicKeyCredential` or authenticator WebIDL at all**, and
  `dom_credential_management_enabled = false` (verified in
  `components/config/prefs.rs`). This is multi-quarter engine work, not a flip. It is a
  **real and growing login-breaker** — Google, Microsoft, Apple, and many banks now push
  passkeys as the default/again-only factor — so 1.0 must lean on saved passwords +
  import + the open-in-system escape hatch and message the limitation plainly. No
  autofill, no built-in translation, no full i18n/IME parity at 1.0.
- **No state partitioning / advanced anti-fingerprinting at 1.0** — Servo's cookie
  partitioning is a literal `// TODO: Apply Partitioning checks` stub
  (`components/net/cookie.rs:383`); CHIPS-style partitioning and fingerprint resistance
  are engine-blocked and scoped as a *tracked, upstream-dependent* goal (§6b), not a
  shipped 1.0 differentiator. swerve's privacy story at 1.0 is **content-blocking + zero
  telemetry + no third-party-cookie-by-default**, not Tor/Brave-class fingerprint
  resistance.
- **No OOPIF / full site-per-process isolation** — process-per-registered-domain is the
  1.0 bar. This **mitigates** universal-XSS-to-full-compromise for the common case but
  does **not** reach the modern cross-origin-isolation bar: with no out-of-process
  iframes and an in-broker network/cookie/compositor, a compromised content process
  still shares an address space with the network stack and the cookie jar
  (Spectre-class + cross-origin-iframe residual, documented as G5 in security.md, punted
  to post-1.0 "Phase C"). The MVP does **not** claim near-Chrome safety.
- **No mobile/Android at 1.0** — see §2a for why this is a real strategic cost, not a
  free deferral.
- **No SwerveOS** — zero engineering spend; narrative only (§9).
- **Not for**: DRM-streaming-only users, enterprise-SSO shops, banking-appliance use,
  "invisible plumbing" users. Windows/macOS are post-1.0 unless explicitly funded.

### 2a. Mobile / Android — analyzed, not hand-waved

The named theming benchmark, **Opera GX, is a mobile-first phenomenon** (its growth and
brand live on phones), and Servo *does* ship an in-tree mobile path
(`ports/servoshell/egl/android` + `egl/ohos` OpenHarmony, the "Kumo" Android shell).
So "defer mobile" is not free — it is a strategic bet that the reachable *initial*
persona (privacy/independence-minded power-customizers, GX refugees on **desktop**)
exists in enough volume to bootstrap, while the larger GX audience is acknowledged as
**out of reach until a mobile port exists**. Honest assessment:

- **Architecture mismatch, not just scope.** swerve's chrome-in-Servo +
  `OffscreenRenderingContext` + winit desktop compositing path is **not** the Android
  EGL/`egl/android` embedding path. A mobile swerve is closer to a **second front-end**
  than a recompile: touch input routing, on-screen keyboard/IME (already engine-weak),
  Android lifecycle, GPU/EGL surface management, and packaging. This is a **multi-person
  -quarter** effort by itself, layered on top of an engine whose Android port is itself
  young.
- **Persona reachability, stated plainly.** Desktop-only is *defensible for v1* because
  the independence/privacy/customization wedge over-indexes on desktop power users — but
  it **does cap the addressable GX-refugee market well below the brand's center of
  gravity**. This is a known ceiling, not an oversight.
- **Decision deferred with a named trigger, not silently dropped.** Mobile is a
  **post-1.0, separately-funded program** with a precondition: Servo's Android shell
  reaching demonstrable daily-driver stability *and* swerve securing Tier-2 headcount.
  Until then, the README and store copy say "desktop browser" without implying a phone
  app is imminent.

---

## 3. Phased roadmap (current → 1.0 → beyond)

Ordered by dependency and risk. The throughline: **stand up CI and the data substrate
before features; the delegate cluster and all UI sit behind the store + internal-pages
+ a security-reviewed dialog track; never build a feature without its persistence
layer; treat the sandbox as the gating deliverable with a pre-decided fallback.**

### Phase 0 — Survival infrastructure (do FIRST, in parallel with everything)
**Goal:** stop flying blind on the #1 existential risk, and make the canary lane cheap
enough to actually run.
- CI with two lanes: a **stable lane** gating every PR against the committed Servo pin,
  and a **nightly canary lane** building against Servo HEAD/latest-monthly so
  embedding-API breaks surface weeks before a migration. swerve has **no CI today** —
  this is the most urgent gap.
- **In the SAME deliverable** (not a follow-up): a **pinned-LLVM/mozjs dev container +
  sccache + a cached/self-hosted runner**. Without the cached/containerized build, the
  nightly canary lane — the plan's core treadmill instrument — is too slow and expensive
  to run, so it silently rots. Quantify and own the build weight: 848 deps, mozjs+mozangle,
  LLVM-pinned; a cold build is the onboarding tax. The dev container is also the
  reproducible-build baseline for §6e.
- **A swerve-owned top-sites compat smoke-suite** (not just WPT): a curated corpus of the
  top-N real sites the target personas actually use, with scripted load + key-interaction
  checks run on the canary lane. This catches **both** embedding-API breaks **and**
  real-site regressions that WPT misses. See §6a.
- Quarantine all `servo::` usage behind a thin `swerve-servo` crate (today inlined in
  `main.rs`); front it with a servo-free, versioned `swerve-protocol` (EngineCommand /
  EngineEvent).
- Cargo workspace decomposition (~10 crates) with `swerve-servo` the ONLY crate that
  may `use servo::*`.
- Written, review-enforced diff-minimization + upstream-first policy; adopt the Servo
  **crates.io LTS train** and a ~6-month scheduled-migration calendar.
- **File the upstream "pluggable sandbox" RFC NOW** (see Phase 5 / §8). It sits on the
  critical path and on Servo's LTS cadence, so the lead time must start in Phase 0.

### Phase 1 — Persistence substrate + first-run + "not broken" web (M6)
**Goal:** make swerve feel like a real browser, let users *move in*, and unblock ~70%
of features. **Hard prerequisite ordering:** the store and `swerve://` internal pages
land first; the delegate cluster and all chrome UI depend on them.
- Per-OS profile dir wired to `Opts.config_dir`; SQLite "places" store via rusqlite
  (bundled), with the **universal sync-record envelope** (uuid, datatype, version
  vector, tombstone, updated_at, encrypted payload) baked into every datatype. **(Hard
  prerequisite for everything below and in Phase 2.)**
- `swerve://` internal-pages framework (registered protocol / `load_web_resource`).
  **(Hard prerequisite for every settings/manager/dialog surface.)**
- **Import-from-other-browsers (P0 adoption-critical):** read Chrome/Chromium, Firefox,
  and Edge profiles — Netscape/JSON bookmarks, `places.sqlite` / Chromium `History`,
  and (where the OS keychain/format permits) saved logins and cookies. This is the
  single biggest determinant of whether a "second browser by choice" gets a second
  launch. Scope realistically: bookmark+history import is tractable; **password import
  is OS-keychain- and DPAPI-gated and is its own sub-track**, not a freebie.
- **Re-estimated delegate cluster — NOT "an afternoon each."** These were previously
  framed as ~11 trivial hooks; that framing is withdrawn. Each requires real,
  un-spoofable, accessible HTML UI in the chrome, round-trip state management, and (for
  the security-sensitive ones) a review pass. They are sequenced **behind** the store +
  internal-pages + a dedicated **security-reviewed-dialog track**:
  - *Cheap, days each:* favicons, clipboard, `notify_crashed` hook.
  - *Real UI, ~1 week+ each:* context menus (accessible, keyboard-navigable),
    form/file/color pickers.
  - *Security-reviewed track, multi-week:* JS dialogs (**must be un-spoofable per spec**
    — origin display, no chrome-impersonation), permission prompts (**must persist to
    the SQLite store** — so they depend on it existing), HTTP auth (credential handling),
    `request_create_new`/popups (popup-abuse heuristics).
- **Enable a real media backend** so `<video>`/`<audio>` decode (present correctness
  bug, not a Servo limitation).
- Flip on + smoke-test the cheap DOM APIs: IntersectionObserver, adoptedStyleSheets,
  FontFace, Web Animations, VisualViewport, Async Clipboard — each behind a WPT gate.
- Move the privileged chrome off `file://` onto the internal scheme; replace
  `evaluate_javascript(format!())` with a typed JSON bridge; fix `js_string`
  U+2028/U+2029 escaping.
- **Field-quality signal, opt-in (resolves the telemetry contradiction):** a local-only
  rolling error log, an opt-in crash reporter that shows the exact payload before any
  send, and the "report this site" plumbing (writes to the local compat log; only
  leaves the machine if the user clicks send). This is the *only* way a verifiably
  zero-telemetry browser learns it is broken in the field — designed in now, not bolted
  on after a support black hole forms.

### Phase 2 — Core product features (M7)
**Goal:** the day-to-day browser. Every item below assumes the Phase 1 store exists.
- Omnibox suggestions/autocomplete (frecency-ranked, URL-vs-search disambiguation).
- Bookmarks (star/folders/bar, Netscape import/export); history recording + searchable
  page; settings UI bound to the store; per-site permissions UI; zoom keybindings +
  per-site memory; full keyboard-shortcut set; session + closed-tab restore.
- **Localization substrate wired through here** (§6c): all new UI strings go through the
  catalog, not string literals — cheap now, a rewrite later.
- Enable + harden **IndexedDB** (rusqlite backend exists, pref-off) — biggest single
  unlock for modern web apps and for offline/Lyku.
- Enable **AccessKit accessibility** (`accessibility_enabled=false` today) + an
  AccessKit adapter for the HTML chrome — cheap early, expensive to retrofit.
- **Engine-blocked P0s with NO primitive — scoped as must-upstream, not solvable-now:**
  - **Downloads:** there is **no download delegate** in libservo. The stopgap intercepts
    via `WebResourceLoad`/`load_web_resource` and **buffers the body** — which **breaks
    on large and streaming files** (memory blowup, no resumable/streamed writes). The
    real fix is **upstreaming a streaming download API to Servo**, which means owning a
    patch across every LTS bump (this *is* the #1 treadmill risk). 1.0 ships the stopgap
    with documented size limits; the upstream API is a funded post-stopgap track.
  - **Find-in-page:** **no find API.** The JS-overlay stopgap is **imperfect and fragile
    on complex DOMs** (shadow DOM, virtualized lists, cross-iframe). Same upstream-or-
    suffer tradeoff as downloads.

### Phase 3 — Theming differentiator (M8)
**Goal:** ship the headline feature, fast and measured.
- `--sw-*` token catalog (primitive/semantic/component) derived from ~4–6 inputs via
  `color-mix()`; refactor `chrome.css` onto it.
- Chrome hot-reload over the existing bridge; live light/dark via `notify_theme_change`;
  force-dark + per-site themes (embedder URL→sheet map, NOT `@-moz-document`).
- `swerve-asset://` handler (path-confined, size/dimension capped) for wallpapers/fonts.
- Validated `.swerve`/`.swervemod` package format (tokens preferred over raw CSS,
  signature verification, capability declarations, installer-recomputed perf score).
- **CI-enforced perf budget**: static-by-default, composited-only animation, RSS +
  frame-time sampler in the Xvfb harness. Build on `filter`/gradients — **not**
  `backdrop-filter` (not wired to Servo's display list at this rev).
- Isolate every Servo theming touchpoint behind one `engine::theming` wrapper.
- **Footprint claim re-baselined here (honest caveat).** The "beat GX on performance /
  lower idle RAM than Chrome" story comes from the *prototype*, which runs media-OFF,
  single-process, accessibility-OFF, and most DOM APIs OFF. By this phase swerve has
  flipped media on, accessibility on, the DOM APIs on, and is heading into
  multiprocess+sandbox (N content processes), IndexedDB, service workers, and a
  tens-of-MB adblock engine. **The perf budget must be re-measured against a 1.0-feature
  build before it is marketed** — market the 1.0 footprint, not the prototype's. The
  CI perf harness exists precisely so this number is measured, not asserted.

### Phase 4 — Content blocking + native add-ons (M9)
**Goal:** the MV3-proof structural win.
- Network blocking via `load_web_resource` + Brave's `adblock` crate (v0.12.5), a
  pre-compiled serialized `Engine` behind `Arc<RwLock>`, **synchronous microsecond
  match on the UI thread** (the net thread `.await`s every request behind one mutex —
  no I/O or list-parsing on that path); never block `is_for_main_frame`.
- Cosmetic filtering + scriptlets via per-tab `UserContentManager`.
- Userscripts as the native add-on type (.user.js, GM_* shim, swerve-side `@match`).
- Per-tab top-frame URL tracking in AppState for correct first-party.
- **Filter-list licensing is a gating legal task, not a footnote** (§6d): EasyList,
  EasyPrivacy, and any default list must have their redistribution terms cleared per
  list *before* they ship in the binary; some require attribution or prohibit
  modification.

### Phase 5 — Security hardening (M10) — release blocker for any public build
**Goal:** make swerve safe for real users. **This is the single least-feasible-solo,
most-likely-to-slip v1 commitment in the plan — re-phased accordingly, with a
pre-decided fallback and a named human precondition.**

- **The 1.0 security bar is explicitly: "sandboxed multiprocess content, on by default,
  on Linux x86-64 only."** macOS-Apple-Silicon (net-new Seatbelt) and Windows
  (AppContainer/Job-Object — "net-new and substantial, 1–2 quarters") sandboxes are
  **post-1.0, with named dependencies**, NOT v1 gates. The previous "on EVERY shipping
  platform" v1 commitment is withdrawn: a solo/tiny team owning a modern per-platform
  sandbox is not credible on the v1 timeline. **The pre-decided fallback is a
  Linux-only 1.0 release** — accepted now, in writing, so it is not a milestone-blowing
  surprise.
- **Linux sandbox, owned:** namespaces + no_new_privs + seccomp-bpf (maintained
  `seccompiler`, modern allowlist) + Landlock — **not gaol 0.2.1 as-is** (verified:
  Servo still pins gaol 0.2.1 in `Cargo.lock`; 21-syscall kill-allowlist, no
  openat2/clone3/rseq, no Windows/Apple Silicon). security.md scopes this as 1–2
  quarters.
- **Upstream dependency on the critical path:** the process-spawn code lives in Servo's
  **constellation**, not the embedder, so making the sandbox pluggable likely **requires
  an upstream Servo change**. The pluggable-sandbox RFC is filed in **Phase 0** so the
  LTS lead time is not the thing that slips 1.0.
- **Human precondition (gate, not aspiration):** **recruiting one co-maintainer is a
  dated precondition for *starting* Phase 5.** This is the most safety-critical,
  least-solo-able work in the plan; a nonce/sandbox/crypto mistake here is silent and
  catastrophic, and bus-factor=1 on it is unacceptable. See §7a.
- Signed auto-update (Authenticode/Developer ID/GPG-minisign) + TUF-style release
  integrity + tested emergency-patch path with kill-switch.
- Local-hash-prefix Safe-Browsing-equivalent (URLhaus/PhishTank/OpenPhish, zero
  per-nav network calls) + dangerous-download/MOTW warnings. **Feed licensing is a
  gating legal task** (URLhaus/PhishTank/OpenPhish terms differ; some restrict
  redistribution) — §6d.
- Baseline security UI: trustworthy origin display, cert interstitial with **no silent
  bypass** (compile `ignore_certificate_errors` out of release), HTTPS-First,
  deny-by-default permission prompts.
- Harden or drop the `SWERVE_IPC` control plane in consumer builds (off by default,
  0600, token auth, per-user runtime dir).
- CI security gates: cargo-deny/audit failing on un-triaged advisories (Servo already
  carries some, e.g. RUSTSEC-2025-0059), PIE/RELRO/BIND_NOW, cargo-fuzz harness.
- **Honest framing of the residual:** process-per-registered-domain removes the
  universal-compromise case but **not** the cross-origin-iframe / Spectre bar (no OOPIF;
  in-broker net+cookies). swerve at 1.0 is "meaningfully safer than single-process,
  below Chrome's site-isolation bar" — and says exactly that.

### Phase 6 — Sync (Lyku) (M11)
**Goal:** the trust feature; sync the substrate built in Phase 1.
- `SyncProvider` trait with self-host server (single Rust binary, SQLite of ciphertext,
  server_seq cursor) + LocalFolderProvider, so real E2EE sync works with **zero Lyku
  dependency**; Lyku is one provider once its auth/blob/zero-knowledge contract is
  confirmed.
- Zero-knowledge by default: Argon2id master key, HKDF-split subkeys,
  XChaCha20-Poly1305 with per-record keys + AAD binding.
- **Crypto-crate reality, stated up front (not buried):** the relevant crates are pulled
  in **at release-candidate versions** pinned by Servo's graph (e.g. argon2 0.6.0-rc.x,
  chacha20poly1305 0.11.0-rc.x, blake2 0.11.0-rc.x). "Crypto is nearly free / already in
  the graph" is **only half true**: building a *zero-knowledge vault — where a
  nonce-reuse or merge bug is silent and catastrophic — on RC crypto whose API can shift
  on every Servo bump* is a real, ongoing risk. Mitigation: one crypto choke-point
  module, pinned crate versions independent of where possible, AAD binding everywhere,
  property-tested merges, and **external crypto review before any vault ships**.
- Per-datatype conflict resolution (VV-LWW settings, OR-Set + fractional-index
  bookmarks/tabs, append-only history, field-LWW vault), convergence property-tested.
- Separate hardened password vault (distinct file, second-factor-gated Vault Key,
  per-entry envelope encryption, zeroize, k-anonymity breach checks).
- Mandatory recovery-key at setup; cursor-based incremental sync over rustls.
- **GDPR / data-controller obligations** for any Lyku-hosted sync are a gating legal
  surface (§6d), even though zero-knowledge minimizes the controller's exposure.

### Phase 7 — 1.0 release polish
**Goal:** ship Linux x86-64 + arm64.
- Live in-product compat page (driven off Servo BWA gaps) + one-click open-in-system.
- "What swerve sends" page; default-browser registration; protocol/deep-link handlers.
- **Top-sites compat smoke-suite promoted to a release gate** (§6a) + WPT regression
  tracking of swerve's target feature set as a per-bump KPI.
- **Manual smoke-test matrix** across a documented set of GPUs/drivers (§6f) and the
  supported distro packages (§6e).
- Enable **WebGL2** (present, pref-off) behind a WPT gate; enable + harden **Service
  Workers** (manager + constellation wiring exist) for PWAs/offline.
- **Localization pass:** ship English + any community-completed locales; verify no
  hardcoded strings remain in chrome (§6c).
- **Distribution:** ship at least Flatpak + a `.deb`/`.rpm` (or AppImage) + an AUR
  recipe, signed, with a reproducible-build attestation from the Phase-0 dev container
  (§6e).

### Beyond 1.0
- **macOS/Apple-Silicon + Windows ports** (sandbox = the largest single deliverable each;
  see Phase 5). Each is its own funded program.
- **Mobile/Android** (§2a) — separately funded, gated on Servo's Android shell maturing.
- State partitioning + anti-fingerprinting **as Servo lands the primitives** (§6b).
- Passkeys/WebAuthn — only after Servo gains `PublicKeyCredential`/authenticator support
  (fund it upstream or wait).
- Cross-origin-iframe isolation ("Phase C") — the actual modern security bar.
- Lyku marketplace; chrome-mod API v1; PDF viewer + print-to-PDF; IME/composition; RTL.
- Fund upstream Servo (anchor positioning, layout gaps, downloads/find APIs, EME hooks,
  WebRTC, pluggable sandbox) — the cheapest durable way to close engine gaps. SwerveOS
  stays a narrative (§9).

---

## 4. Prioritized feature backlog (P0 = required for 1.0)

| Pri | Feature | Status today | Blocker / path |
|-----|---------|--------------|----------------|
| P0 | CI (stable + canary) + pinned dev-container + sccache + top-sites smoke-suite | none | build infra |
| P0 | `swerve-servo` quarantine + `swerve-protocol` | inlined in main.rs | refactor |
| P0 | Profile dir + `config_dir` + SQLite places store | zero persistence | app arch (hard prereq) |
| P0 | `swerve://` internal-pages framework | none | ProtocolHandler (hard prereq) |
| P0 | **Import bookmarks/history/passwords/cookies from Chrome/FF/Edge** | none | first-run; keychain/DPAPI for passwords |
| P0 | Delegate cluster (menus, dialogs, pickers, auth, popups, favicons, crash) | 5/38 wired | **days-to-weeks each**, behind store + dialog-review track |
| P0 | Media backend (video/audio decode) | OFF (bug) | enable feature |
| P0 | Cheap DOM APIs (IO, adoptedStyleSheets, FontFace, WebAnim, VisualViewport, AsyncClipboard) | pref-off | flip + WPT gate |
| P0 | Opt-in crash report + local error log + "report this site" | none | resolves telemetry tension |
| P0 | Omnibox suggestions (frecency) | none | needs store |
| P0 | Bookmarks / history / settings / permissions UI | in-memory/none | needs store |
| P0 | Session + crash + closed-tab restore | none | `notify_crashed` |
| P0 | Zoom keybindings + per-site memory | none | `set_page_zoom` |
| P0 | Downloads + manager | none | **engine-blocked, no primitive** (stopgap buffers; must upstream streaming API) |
| P0 | Find-in-page | none | **engine-blocked, no primitive** (JS overlay fragile; must upstream) |
| P0 | Content blocker (adblock-rust, on by default) | none | clean crate add + list licensing |
| P0 | Theming tokens + hot-reload + light/dark + perf budget | chrome is HTML | low |
| P0 | Sandboxed multiprocess content — **Linux x86-64 only** | single-process | **hard; gating; Linux-only-v1 fallback pre-decided** |
| P0 | Chrome off `file://` + typed JSON bridge | file:// today | refactor |
| P0 | Signed auto-update + release integrity | none | new subsystem |
| P0 | Safe-Browsing-equivalent (local hash prefix) | none | feed licensing (gating) |
| P0 | AccessKit accessibility | OFF | activate + adapter |
| P0 | Default-browser + protocol handlers | none | `request_protocol_handler` |
| P0 | Localization substrate (no hardcoded strings) | hardcoded English | string catalog |
| P0 | Distribution: Flatpak + .deb/.rpm/AppImage + AUR, signed | none | packaging per distro |
| P1 | IndexedDB (hardened) | pref-off | enable + WPT gate |
| P1 | Force-dark + per-site themes + `swerve-asset://` | none | UserContentManager |
| P1 | Package format + sideload | none | manifest + signing |
| P1 | Userscripts (GM_* shim) | none | UserContentManager |
| P1 | Sync: SyncProvider + self-host + E2EE settings/bookmarks/history | none | needs store first; RC-crypto risk |
| P1 | Password vault | none | needs E2EE design + external review |
| P1 | Private/incognito window (real isolation) | none | engine investigation |
| P1 | Service Workers (PWA/offline) | pref-off | enable + harden |
| P1 | WebGL2 | pref-off | enable + WPT gate |
| P1 | Compat page + open-in-system escape hatch | none | low |
| P2 | Chrome-mod API v1 + marketplace | none | post-1.0 |
| P2 | PDF viewer + print-to-PDF | none | bundle JS/WASM |
| P2 | IME/composition input + RTL | raw keys only | `InputMethodControl`; engine-blocked |
| P2 | Windows / macOS ports | Linux-only | sandbox per platform (post-1.0) |
| P2 | Mobile/Android | none | separate front-end; gated on Servo Android shell |
| P2 | State partitioning + anti-fingerprinting | engine stub (`TODO`) | **engine-blocked**; upstream/wait |
| P2 | Passkeys/WebAuthn | absent WebIDL, pref-off | **engine-blocked, multi-quarter**; fund/wait |
| P2 | WebRTC, HTTP/3, EME/DRM, autofill, translation | absent/off | fund/upstream/never |

---

## 5. Engine gap analysis (condensed). Full: [`engine-gap.md`](plan/engine-gap.md)

Servo's **cores are production-grade**: Stylo 0.18 (Firefox's CSS cascade),
SpiderMonkey mozjs_sys 140.x (full JIT), WebRender 0.69 — CSS parse + JS exec are at/
near Chrome. The gap is everywhere *between* the cores.

| Gap | Severity | Plan |
|-----|----------|------|
| Media backend OFF (no video/audio) | P0 correctness bug | **Enable** media-gstreamer/system backend |
| IntersectionObserver, adoptedStyleSheets, FontFace, WebAnim, VisualViewport, AsyncClipboard | P0, implemented-off | **Flip on**, WPT-gate each |
| IndexedDB | P1, off | **Enable + harden** (rusqlite backend exists) |
| AccessKit a11y | P0 compliance | **Activate** |
| WebGL2 | P1, off | **Enable** behind WPT gate |
| Service Workers | P1, off | **Enable + harden** |
| Downloads, find-in-page | P0, **no libservo primitive** | Stopgap (buffer/overlay, both fragile) then **upstream** (= owns a patch on the treadmill) |
| Cookie/state partitioning (CHIPS) | P2, **engine stub** (`cookie.rs:383 // TODO`) | **Wait/fund** upstream; not a 1.0 differentiator |
| Passkeys/WebAuthn (`PublicKeyCredential`/authenticator WebIDL absent) | P2, **engine-blocked** | **Wait/fund** upstream — multi-quarter, not a flip |
| Anchor positioning | hard-disabled (`unreachable!`) | **Wait/fund** upstream — do NOT fork layout |
| Subgrid, view-transitions, masonry | `unimplemented!` | **Wait/fund** upstream |
| Vertical/RTL writing modes | panic-guarded | **Wait/fund** upstream (blocks RTL i18n) |
| EME/DRM (MediaKeys absent) | dealbreaker for streaming | **Out of scope v1**; Widevine partnership later or never |
| WebRTC | off, immature | **Defer**; fund/upstream later |
| HTTP/3/QUIC | absent (H1+H2 only) | **Accept** (H2 fallback); not a correctness blocker |
| WebExtensions/MV3 + CDP/DevTools | absent | **Workaround** — native add-ons + WebDriver, not parity |

**Quantitatively (pinned to one source, verified 2026-06-18):** ~62% overall WPT;
**87 of 439 categorized Baseline-Widely-Available features at production quality =
19.8%** (333 partial = 75.9%, 19 unsupported = 4.3%; **593 features in the BWA catalog
total** — 593 is the catalog size, NOT the percentage denominator; 87/593 would be
14.7%). An external linear projection puts the plateau near ~80% by ~2037 at ~13 FTE —
**directional only, 11 years out, not authoritative.** **Every pref-enable must clear a
swerve-defined WPT bar** — flipping naively ships bugs users blame on swerve. The
value-add over raw Servo is a hardened, verified default profile, not Servo's
everything-off defaults.

---

## 6. Subsystem design summaries

### Theming — the headline. Full: [`theming.md`](plan/theming.md)
The HTML chrome is a genuine superpower: a theme apply is a `setProperty` on `:root`
over the existing bridge (<16ms, no Servo change). Content theming is verified real:
`UserContentManager` injects user stylesheets/scripts; `notify_theme_change(Dark)`
flips `prefers-color-scheme` live. Three hard constraints: injected stylesheets need a
page reload (use preview-then-persist), `@-moz-document` is disabled (per-site is
embedder-driven by URL), and `backdrop-filter` is **not** wired to the display list
(build on `filter`/gradients). The differentiator is **measured performance** vs GX's
650MB–1.2GB / 80–100% CPU — enforced in CI, **measured against the 1.0-feature build,
not the prototype** (§3 Phase 3). Isolate every Servo touchpoint behind one
`engine::theming` module.

### Sync (Lyku) — the trust feature. Authoritative: [`sync-lyku-integration.md`](plan/sync-lyku-integration.md) (grounded in the real Lyku codebase); earlier generic exploration: [`sync.md`](plan/sync.md)
**Lyku is real** — the user's platform (lyku.org, source at `/raid/lyku`): Bun + Postgres +
Redis + OpenSearch + NATS + Cloudflare R2, with the `lockstep-core`/`pg-models`/`mapi-models`
type-safe framework. It is a near-ideal backend and already provides opaque session tokens +
scoped `lyk_` API keys + OAuth2/OIDC, an R2 presigned-upload flow, MessagePack/NATS, and —
crucially — a generic **`synced<T>` replication framework** that is exactly the delta-sync
cursor primitive. swerve sync rides those rails: ~4 new `pg-models` tables + ~11 `mapi-models`
routes + `read/write:sync` scopes, reusing `core-service` auth. Still pluggable: a
`SyncProvider` trait (Lyku + self-host + local-folder) keeps Lyku optional. **Crypto caveat:**
Lyku is *passwordless*, so zero-knowledge E2EE can't derive from a login password — it needs a
**separate sync passphrase** (Argon2id → 2-tier key hierarchy → XChaCha20-Poly1305; Lyku stores
only ciphertext). Servo's graph supplies the crypto crates but at RC versions whose APIs shift
on Servo bumps → one choke-point module, pinned, property-tested merges, external review before
any vault ships. Sync is a replication layer over a local store **that does not exist yet** —
build each feature's store with the sync envelope from day one; never bolt sync on later.

### Security & sandboxing — the release blocker. Full: [`security.md`](plan/security.md)
swerve is single-process and unsandboxed today, with a privileged `file://` chrome
receiving web-controlled strings — disqualifying for real users. The most load-bearing
pre-release requirement is sandboxed multiprocess content (≥ process-per-registered-
domain) — and it is the **single most-likely-to-slip v1 deliverable**, so the 1.0 bar is
**Linux-x86-64-only, on by default**, with a **pre-decided Linux-only-v1 fallback**,
macOS/Windows post-1.0, and a **co-maintainer hire as a dated precondition**. The engine
code exists in Servo but is OFF, partly stubbed, and built on the unmaintained **gaol
0.2.1** (verified still pinned; brittle 21-syscall kill-allowlist, no Windows/Apple
Silicon). The spawn code lives in Servo's **constellation**, so a pluggable sandbox
likely needs an **upstream RFC — filed in Phase 0**. Network security is sound and
inherited (rustls+aws-lc-rs, HSTS, CSP, CORS, SRI). Residual: no OOPIF / in-broker
net+cookies = below Chrome's isolation bar; documented as G5/Phase-C, never claimed as
near-Chrome safety.

### Extensions & content blocking — the MV3-proof win. Full: [`extensions.md`](plan/extensions.md)
First-class content blocking needs no fork: `load_web_resource` (per-request
interception) + `UserContentManager` + Brave's `adblock` crate (v0.12.5, MPL-2.0, clean
add) ships network + cosmetic blocking in low-thousands of lines. **WebExtensions parity
is NOT realistic** (zero extension infra in Servo, multi-person-year, a fork-magnet —
the Verso treadmill). Path: content-blocking first, then userscripts (native add-on
type), then a small native `swerve.*` API — deliberately **not** `chrome.*`. The
dominant risk is performance: the block decision runs on the UI thread while net
`.await`s every request behind one mutex — it must be a synchronous in-memory match
against a pre-compiled engine, nothing else. Default-list **redistribution licensing is
a gating task** (§6d).

### 6a. Testing / QA / web-compat regression strategy (new)
WPT-gating each pref is necessary but **not sufficient** — it does not tell you whether
the actual sites your users visit work. The plan adds:
- **A swerve-owned top-sites compat corpus:** the top-N real sites the personas use
  (search, mail, docs, social, dev tools, media-non-DRM, banking-non-app), each with a
  scripted load + key-interaction smoke check. Runs on the **canary lane** (catches
  embedding-API breaks *and* real-site regressions) and is a **Phase-7 release gate**.
- **A manual smoke-test matrix** across the documented GPU/driver set (§6f) and the
  supported distro packages (§6e), run per release-candidate.
- **The no-telemetry-compatible field signal** (Phase 1): opt-in crash reports, a
  local-only error log, and a "report this site" button — the *only* way a verifiably
  zero-telemetry browser learns what is broken in the field. This **explicitly resolves**
  the "verifiable zero telemetry vs knowing the browser is broken" tension that the prior
  plan never reconciled: nothing leaves the machine without an explicit click and a
  visible payload.
- **Beta/nightly channels** so regressions surface on opt-in users before stable.

### 6b. State partitioning & anti-fingerprinting (new; engine-blocked)
For a privacy-positioned browser, partitioned storage (CHIPS) and fingerprint
resistance are table-stakes against Brave/Mullvad/Tor — but they are **engine-blocked**:
Servo's cookie partitioning is a literal `// TODO: Apply Partitioning checks`
(`components/net/cookie.rs:383`), and there is no fingerprint-resistance layer. **Honest
stance:** swerve's 1.0 privacy story is **content-blocking on by default + verifiable
zero telemetry + no third-party cookies by default + the SameSite behavior Servo
provides** (audited, not assumed) — *not* Tor-class fingerprint resistance, which is a
**post-1.0, upstream-dependent** goal. Marketing must not imply Brave/Tor parity here.

### 6c. Localization / i18n (new)
The chrome is hardcoded English today. For an EU-grant-funded (NLnet/STF) project,
localization is frequently a **funding condition**, so it is treated as a substrate, not
a P2 afterthought: a **string catalog wired in Phase 2** (no literals in new UI), with
English + any community-completed locales shipping at 1.0. **RTL and IME remain
engine-blocked** (vertical/RTL writing modes are panic-guarded; IME composition is
weak), so the honest near-term reality is **"works well for LTR locales; RTL and IME are
stretch goals pending upstream"** — stated, not buried.

### 6d. Legal / licensing / trademark / privacy-policy (new; gating, not optional)
For anything shipped publicly these are **release-gating**:
- **Filter-list redistribution licenses** (EasyList, EasyPrivacy, any default list) —
  cleared per list before bundling; some require attribution or forbid modification.
- **Safe-Browsing feed licensing** (URLhaus, PhishTank, OpenPhish) — terms differ; some
  restrict redistribution of the hash sets. Verify before shipping the local DB.
- **Widevine/EME reality** — out of scope; if ever pursued, Widevine licensing is a
  Google-controlled gate, which cuts against the independence thesis.
- **GDPR / data-controller obligations** for Lyku-hosted sync — even zero-knowledge sync
  has a controller; a privacy policy + data-processing posture is required before Lyku
  launches.
- **The "swerve" trademark** — clear the name (and "Lyku") before public launch;
  rename cost grows with adoption.

### 6e. Distribution / packaging (new; quantify the burden)
The build is **848 deps + LLVM-pinned mozjs/mozangle + bundled ANGLE** — large binary,
large package, heavy per-distro work. Plan:
- **Targets at 1.0:** Flatpak (sandboxed, distro-agnostic) + a `.deb`/`.rpm` or AppImage
  + an AUR recipe — all **signed** (GPG-minisign / Authenticode-equivalent path tied to
  the §3 auto-updater).
- **Reproducible-build commitment** anchored to the **Phase-0 pinned dev container**, so
  third parties can verify the binary matches source — a concrete privacy/independence
  proof point.
- **App-store realities** (macOS notarization, Microsoft Store) are post-1.0, tied to the
  respective platform ports.
- **Binary/package size is a tracked CI metric** alongside the perf budget, because the
  LLVM-pinned engine makes "small" a non-trivial promise.

### 6f. GPU / driver / surfman portability (new; a real "won't start" cause)
surfman + ANGLE + WebRender initialization across the long tail of user GPUs/drivers is
a **top real-world cause of "the browser won't start"** — and a telemetry-free swerve
**cannot passively observe it**. Plan:
- A **documented supported-GPU/driver matrix** + a manual smoke pass (§6a) on a spread of
  Intel/AMD/NVIDIA + Mesa/proprietary stacks before each release.
- A **software/llvmpipe fallback path** and a clear first-run error page (a `swerve://`
  internal page) when hardware GL/ANGLE init fails — so a driver failure produces an
  actionable message + a "report this" hook, not a silent crash.
- The opt-in crash/error signal (§6a) carries GPU/driver strings *only with consent*, so
  the field-visibility gap here is mitigated without ambient telemetry.

---

## 7. Sustainability & maintenance strategy. Full: [`sustainability.md`](plan/sustainability.md)

**The Verso lesson:** archived Oct 8 2025 for inability to track Servo's churn while
embedding the **low-level** way (~30 component crates, a 2,200-LOC self-owned
compositor). swerve embeds the **high-level** way (umbrella `servo` crate, ~30 public
symbols, pinned rev, no compositor) — the architectural insurance is already paid. **But
every "must-upstream" item (downloads, find, pluggable sandbox) deliberately re-incurs
this risk by making swerve own a patch across LTS bumps — the plan accepts this trade
consciously and minimizes the count.**

**The strategy is process discipline:**
1. **CI now** (stable + canary lanes) **+ the pinned dev-container + sccache** in the
   same deliverable — the canary lane is useless if it's too slow to run.
2. **Adopt the Servo crates.io LTS train** — one scheduled, reviewed migration every
   ~6 months instead of chasing HEAD. The single biggest risk reducer.
3. **Quarantine** all `servo::` in one crate; firewall the rest behind `swerve-protocol`.
4. **Diff-minimization + upstream-first**, review-enforced: public-API-only, minimal-
   patch steady state, every gap becomes a Servo PR not a private fork. **File the
   pluggable-sandbox RFC in Phase 0.**
5. **Honest scope** in the README: "usable not universal", "Chrome parity = never",
   "desktop-only", "no DRM/passkeys/RTL at 1.0".
6. **Funding aligned with the no-telemetry ethos**: Sponsors/Open Collective now, Lyku
   subscription as the Tier-2 flywheel, EU grants (NLnet/NGI, Sovereign Tech Fund) — no
   ads/affiliate/telemetry. See §7a for the headcount reconciliation.
7. **Recruit a co-maintainer before committing past Tier-1** — bus-factor=1 is the
   second existential risk, and it is a **hard gate on Phase 5** (§7a).

Accept the trades: LTS lags HEAD by up to 6 months (features feel stale), and swerve
can never outrun Servo's ~93%-of-tests-it-runs ceiling.

### 7a. Funding / headcount reconciliation (gate, not a risk-row)
The prior plan listed tier *ranges* ($0–250k / $0.6–1.5M / $5–15M) but never reconciled
them with the phase scope or with the secured funding. Pinned now:

- **Secured today: Tier-1 = $0 / solo (bus-factor 1).** The roadmap implies **years of
  full-time work**; that time is currently unfunded and competes with the maintainer's
  other obligations. This is stated, not implied.
- **What Tier-1/solo can realistically do:** Phase 0 (CI/container), Phase 1 (store +
  first-run + import + the cheap delegate/DOM work + the field signal), Phase 2 core
  features, Phase 3 theming, Phase 4 content blocking. I.e. **a private, themeable,
  ad-blocking, importing, single-process (or best-effort-sandboxed) Linux build** — a
  credible *preview*, not a safe public 1.0.
- **What requires Tier-2 (≈3–6 people) and is DEFERRED INDEFINITELY if funding stays
  Tier-1:** Phase 5 (the per-platform sandbox + signed update + Safe-Browsing — the
  safety-critical, least-solo-able work), Phase 6 (E2EE sync + the password vault, where
  a crypto mistake is catastrophic), and the macOS/Windows/mobile ports. **A public,
  security-claiming 1.0 is gated on Tier-2.**
- **Dated precondition:** **"recruit one co-maintainer" is a precondition for *starting*
  Phase 5** — not an aspiration. If no co-maintainer is secured, swerve ships as an
  explicitly-labeled preview/beta and does **not** make safety claims.
- **Tier-3 ($5–15M / upstream-funding) is the only path** to closing engine gaps
  (downloads/find/sandbox/anchor/EME) on swerve's own timeline rather than Servo's.

---

## 8. Ranked risk register

| # | Risk | L × I | Mitigation |
|---|------|-------|-----------|
| 1 | **Servo-sync treadmill** (killed Verso); *amplified* by every must-upstream patch | Med × Critical | LTS train, quarantine crate, canary CI, upstream-first, minimize patch count |
| 2 | **Funding stuck at Tier-1 while M10–M11 need Tier-2** — safety/sync work never safely ships | High × Critical | §7a gate: Phase 5/6 deferred + co-maintainer precondition; ship labeled preview otherwise |
| 3 | **Bus-factor = 1** | High × Critical | Recruit a co-maintainer; **hard gate on Phase 5** |
| 4 | **Sandbox is the gating, most-likely-to-slip v1 item** (Servo-owned spawn + gaol 0.2.1 + per-platform) | High × Critical | **Linux-x86-64-only-v1 bar + pre-decided Linux-only fallback**; pluggable-sandbox RFC filed Phase 0 |
| 5 | **No CI today** — late breakage; canary too slow to run without a cached build | High × High | Phase 0: CI + pinned dev-container + sccache **as one deliverable** |
| 6 | **Persistence-before-features inversion** — UI before the store guarantees rework | High × High | Store + sync envelope first; delegate cluster + UI sequenced behind store + internal-pages |
| 7 | **Engine-blocked P0s with no primitive** (downloads buffer-breaks, find fragile) sit on critical path + must-upstream | High × High | Documented stopgaps with limits; funded upstream track; feeds risk #1 consciously |
| 8 | **Crypto correctness on RC crates** (nonce reuse / lost-password loss is silent + catastrophic) | Low × Critical | One choke-point module, pinned crates, AAD binding, property-tested merges, **external review before vault ships** |
| 9 | **Adoption blocker: can't import / first-run friction** — a "second browser" nobody migrates into | High × High | Import (bookmarks/history/passwords/cookies) is a Phase-1 P0 + first-run funnel |
| 10 | **"Verifiable zero telemetry" vs no field visibility** — broken-in-field with no signal | High × Med | Opt-in crash report + local error log + "report this site"; nothing sent without explicit click |
| 11 | **Footprint marketed off the prototype** (media/a11y/multiprocess/adblock OFF) | Med × Med | Re-baseline perf vs the 1.0-feature build in CI before marketing |
| 12 | **GPU/driver/surfman init fails on the long tail** — "won't start," invisible to a telemetry-free browser | Med × High | Supported-GPU matrix + llvmpipe fallback + actionable error page + consented GPU strings in crash report |
| 13 | **Web-compat breakage churns users** (62% WPT, 19.8% BWA) | High × Med | Top-sites smoke corpus + compat page + open-in-system; honest messaging |
| 14 | **adblock UI-thread perf** stalls all fetches | Med × High | Synchronous pre-compiled in-memory match only |
| 15 | **Scope creep toward Chrome parity** burns the tiny team | Med × High | Enforce non-goals; "second browser by choice" |
| 16 | **Build weight** (848 deps, mozjs/ANGLE, LLVM pinning) taxes CI + onboarding + package size | High × Med | sccache, fat caches, dev container; package-size as a CI metric |
| 17 | **Legal/licensing gate missed** (filter lists, SB feeds, trademark, GDPR) blocks public ship | Med × High | §6d cleared per item before the binary ships |
| 18 | **No passkeys at 1.0** breaks a growing share of logins (Google/MS/Apple/banks) | High × Med | Saved passwords + import + open-in-system; message clearly; fund/wait upstream |
| 19 | **Ladybird out-executes the independence story** | Med × Med | Differentiate on theming/experience, not conformance |
| 20 | **Mobile-absent caps the GX-refugee market** | Med × Med | Stated ceiling (§2a); post-1.0 funded program gated on Servo Android shell |
| 21 | **Servo loses funding / slows** (Igalia-dependent) | Low-Med × Critical | Fund upstream; diversify; accept inheritance |
| 22 | **Lyku crypto root** — Lyku is passwordless, so zero-knowledge E2EE needs a *separate* sync passphrase (UX friction; lost passphrase = lost vault, silently) | Med × High | Clear recovery UX + explicit warnings; pluggable SyncProvider (self-host/local-folder) optional; see [`sync-lyku-integration.md`](plan/sync-lyku-integration.md) |

---

## 9. SwerveOS note. Full: [`swerveos.md`](plan/swerveos.md)

A Rust OS with swerve as primary UI is conceivable (Servo runs on Redox as of Oct
2025) but its cost is dominated by **kernel + drivers**, not the browser (~5–10% of the
effort). Reality check: after ~10 years Redox still has no Wi-Fi/BT, Intel-only GPU
accel, most touchpads unsupported, and its Servo port crashes on the second page.
ChromeOS and webOS both run **on the Linux kernel** precisely to inherit drivers; a
from-scratch Rust OS discards that one shippability asset. **Recommendation: zero
engineering spend now** — it would multiply the treadmill that killed Verso. Keep it as
a free narrative. If ever built, make it a **swerve-as-shell immutable Linux kiosk
image** that reuses Linux drivers — not a from-scratch OS — and only for captive/known
hardware (kiosk, signage, thin client, OEM smart-TV).

---

## 10. Open decisions needed from the user

1. **v1 target-site list / persona** — mainstream non-DRM web vs literal Chrome parity?
   Pins capability scope and decides whether EME/WebRTC/extensions are de-scoped or
   funded; also pins the §6a top-sites compat corpus.
2. **DRM stance** — ship without it and message clearly (recommended), pursue a Widevine
   partnership later (with the independence-thesis cost), or treat DRM streaming as
   permanently out of scope?
3. **Upstream-funding posture** — will swerve fund upstream Servo (downloads/find APIs,
   pluggable sandbox, IndexedDB, WebGL2, media, partitioning, passkeys, EME hooks), and
   at what FTE? It is the cheapest durable way to close gaps and de-risk the treadmill —
   and it is the only path to a public, safety-claiming 1.0 (§7a).
4. **Linux-only v1, confirmed?** — the plan pre-decides Linux-x86-64-only sandboxing as
   the 1.0 bar with a Linux-only fallback. Confirm, or commit the funding for the
   net-new macOS-Apple-Silicon / Windows-AppContainer sandboxes that move them onto the
   critical path.
5. **Co-maintainer / bus-factor timeline** — Phase 5 cannot start solo. What is the
   dated plan to recruit one co-maintainer, and does a public 1.0 wait on it (§7a)?
6. **Lyku sync specifics** (now characterized from the codebase — see
   [`sync-lyku-integration.md`](plan/sync-lyku-integration.md)): (a) accept OAuth bearers at
   the MessagePack gateway, or keep the OIDC→`lyk_`-API-key two-step? (b) a *separate sync
   passphrase* — acceptable UX, given it's the unavoidable price of zero-knowledge on a
   passwordless account? (c) `bytea` ciphertext (first production use in `pg-models`) vs
   base64-in-`text`? (d) the idiomatic `synced<T>` path (not yet wired into a live service)
   vs a bare NATS listener for v1? (e) per-row vs per-account-monotonic `sequence` for delta
   pull? (f) build the (cheap, idiomatic) Lyku-side surface now, before swerve has any local
   store? Plus the GDPR data-controller posture (§6d).
7. **Sync scope + defaults** — which datatypes sync (bookmarks/history/passwords/
   settings/tabs), and is history-sync default-on (Chrome parity) or off (privacy-first)?
   Shapes the store schema, so decide before building the store.
8. **Downloads & find-in-page** — ship the fragile stopgaps (buffered downloads with size
   limits, JS-overlay find) at 1.0, or fund upstreaming real Servo APIs (adds a
   carried-patch to the treadmill but removes the fragility)?
9. **Field-quality signal design** — confirm the opt-in crash report + local error log +
   "report this site" model (nothing sent without an explicit click and a visible
   payload) is the agreed reconciliation of "verifiable zero telemetry" with knowing the
   browser is broken (§6a).
10. **Import scope** — bookmarks+history only (tractable solo), or also passwords +
    cookies (OS-keychain/DPAPI work, its own sub-track) at 1.0?
11. **Localization commitment** — is i18n a funding condition (likely for NLnet/STF)? If
    so, the string catalog (§6c) is non-negotiable substrate and locales must be
    resourced.
12. **Mobile** — accept the desktop-only persona ceiling (§2a) for 1.0, or fund a mobile
    front-end program (gated on Servo's Android shell maturing)?
13. **Servo rev** — stay pinned at `ed1af70` or bump toward 0.2.0+/the LTS release to
    pick up color-mix n-color, @layer inspector, font-fallback fixes, accepting the
    embedding-API breakage?
14. **Distribution / go-to-market** — concrete channel to reach the power-customizer /
    anti-Google audience (HN, Phoronix, FOSDEM, independence sponsorship), and the
    1.0 package set (Flatpak/.deb/.rpm/AUR — §6e)?
