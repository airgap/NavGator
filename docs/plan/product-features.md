# swerve — Browser Product Feature Inventory & Gap vs. M1–M5

> Scope: everything a full browser needs as a **product** (not engine internals),
> gapped against swerve's verified current state (repo at `/raid/swerve`, Servo rev
> `ed1af70`). Each feature is tagged **have / partial / missing** with a target
> **priority** (P0 = required for a credible 1.0, P1 = expected by power users / fast
> follow, P2 = differentiator or later).
>
> Ground truth used: `src/main.rs` (788 lines), `src/chrome/{index.html,chrome.css,chrome.js}`,
> `docs/ARCHITECTURE.md`, and the cached Servo source at
> `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70/`
> (`components/servo/webview_delegate.rs`, `webview.rs`, `servo_delegate.rs`,
> `components/shared/embedder/lib.rs`, `components/net/resource_thread.rs`,
> `ports/servoshell/desktop/dialog.rs`).

---

## TL;DR — the shape of the gap

swerve M1–M5 built the **shell and the compositing/bridge plumbing**: HTML chrome,
content webviews, tabs, omnibox-driven navigation, an input router, and an external
IPC control plane. That is a real browser *frame*. But essentially **none of the
product features** that make a browser usable day-to-day exist yet, and — crucially —
**libservo does not give them to you for free**. The engine exposes the *hooks*
(delegate callbacks and a handful of `WebView` methods); the embedder must build the
storage, the UI, and the policy on top.

Three structural facts dominate this whole dimension:

1. **No persistence layer exists at all.** swerve writes nothing to disk — no
   bookmarks, history, downloads, settings, passwords, sessions. Even Servo's own
   cookie jar is ephemeral, because swerve never sets `config_dir`
   (`ServoBuilder::default()` in `main.rs:544`; Servo only persists `cookie_jar.json`
   when a config dir is configured — `components/net/resource_thread.rs:213,672`).
   **A profile/data store is the precondition for ~70% of this list** and should be
   the very next thing built.

2. **libservo gives hooks, not features.** The good news: at rev `ed1af70` the
   `WebViewDelegate` trait is rich — `request_permission`, `request_authentication`,
   `request_create_new` (popups/`window.open`), `request_protocol_handler`,
   `show_embedder_control` (select/file/color/datetime/IME/**context menu**/simple
   dialogs), `notify_favicon_changed` + `WebView::favicon()`, `notify_crashed`,
   `notify_fullscreen_state_changed`, full **AccessKit** accessibility tree updates,
   `WebView::set_page_zoom`, and `WebView::take_screenshot`. The bad news: swerve
   implements only **4 of ~40** delegate methods today (`notify_new_frame_ready`,
   `notify_url_changed`, `notify_page_title_changed`, `notify_history_changed`,
   `request_navigation`). Every unimplemented method is a visible product bug waiting
   to happen (a `confirm()` dialog silently does nothing; a `<select>` doesn't open;
   `window.open` is a no-op; a download just fails).

3. **Some marquee features the engine does NOT provide and swerve must build itself
   (or do without):** **find-in-page**, **downloads** (no download delegate exists at
   all in libservo at this rev — searched `components/servo/`, `components/net/`; only
   a `fetch` `Initiator::Download` enum value), **PDF viewing**, **reader mode**, and
   **a real private/incognito profile boundary**. These are genuinely expensive and
   several are blocked on engine work, not just UI work.

If you ship M1–M5 as-is and call it a browser, the first five things a user tries —
right-click, download a file, find text on a page, log in (saved password / autofill),
and reopen the window (session restore) — **all fail or no-op**. That's the priority
signal.

---

## Master feature table (prioritized)

Status legend: ✅ have · 🟡 partial · ❌ missing · 🧱 *engine-blocked* (libservo at
`ed1af70` lacks the primitive; needs Servo work or a heavy embedder workaround).

| # | Feature | Status | Priority | Current state in swerve | What's needed |
|---|---------|--------|----------|-------------------------|---------------|
| 1 | **Omnibox: URL parse + go** | 🟡 | P0 | `chrome.js:101-112`: trims, `https://` prefix, DuckDuckGo fallback. Heuristic-only. | Proper URL/term disambiguation, IDN/punycode, localhost/IP, `scheme:` passthrough. |
| 2 | **Omnibox: suggestions/autocomplete dropdown** | ❌ | P0 | None. No dropdown, no inline autocomplete. | A suggestion UI + ranked source merge (history/bookmarks/search). Needs #14/#9/#10 stores. |
| 3 | **Omnibox: history-weighted ranking** | ❌ | P1 | None. | Frecency scoring over history+bookmarks (visit count × recency decay), prefix/host match. |
| 4 | **Omnibox: search-engine config + suggest API** | 🟡 | P1 | Hardcoded DDG (`chrome.js:110`). | Engine list, default selection, `%s` templates, keyword search, optional suggest endpoint. |
| 5 | **Bookmarks: add/remove/star** | ❌ | P0 | None. No store, no star UI. | Data store (#store) + star button + manager page. |
| 6 | **Bookmarks: folders / bookmarks bar** | ❌ | P1 | None. | Folder tree model, bookmarks-bar strip in chrome, drag-org. |
| 7 | **Bookmarks: import/export (HTML/Netscape)** | ❌ | P1 | None. | Parser/serializer for the Netscape bookmark format (Chrome/FF interop). |
| 8 | **History: record visits** | 🟡 | P0 | `notify_url_changed`/`notify_history_changed` fire (`main.rs:466,483`) but data is **per-tab in-memory only**, lost on close. | Persist `(url,title,visit_time,transition)` rows to the store. |
| 9 | **History: UI + search** | ❌ | P0 | None. | A `swerve://history` page with text search + delete + clear-range. |
| 10 | **Downloads: trigger + save to disk** | 🧱❌ | P0 | None. A download today just fails silently — **libservo exposes no download delegate** at `ed1af70`. | Intercept attachment responses via `load_web_resource`/`WebResourceLoad` or add an engine download path; stream to disk. Hard. |
| 11 | **Downloads: manager UI** | ❌ | P0 | None. | `swerve://downloads` list, per-item progress, open/show-in-folder, cancel. |
| 12 | **Downloads: resume / pause** | 🧱❌ | P2 | None. | HTTP Range + persisted partial state. Depends on #10 path. |
| 13 | **Profiles / multi-account** | ❌ | P1 | None. Single ephemeral session. | Profile dirs (config_dir per profile), profile switcher, isolated cookie/storage. |
| 14 | **Profile/data store (the substrate)** | ❌ | **P0** | None whatsoever — nothing persists. | SQLite (recommend `rusqlite`) + an `XDG`/per-OS data dir; migrations. **Build first.** |
| 15 | **Password manager (store/fill)** | ❌ | P1 | None. | Encrypted credential store (OS keyring), capture-on-submit, fill-on-load via injected script (`UserContentManager`). |
| 16 | **Autofill: forms / addresses / cards** | ❌ | P2 | None. | Profile data + field-heuristics + fill UI. Large surface. |
| 17 | **Passkeys / WebAuthn** | 🧱❌ | P2 | None. | WebAuthn is an engine/DOM capability; verify Servo support (likely absent) before promising. Engine-blocked. |
| 18 | **Find-in-page (Ctrl+F)** | 🧱❌ | P0 | None. **No find API in libservo** (`webview.rs` has no `find_*`; searched whole `components/servo/`). | Either land find in Servo, or inject a JS find-highlight overlay via `UserContentManager` (imperfect). |
| 19 | **PDF viewing** | 🧱❌ | P1 | None. PDFs would download, not render. | Servo has no PDF renderer; bundle a WASM/JS viewer (e.g. pdf.js-equivalent) served via internal scheme, or native. |
| 20 | **Reader mode** | ❌ | P2 | None. | Article extraction (Readability-style) + a styled reader page; inject via internal scheme. Pure embedder work. |
| 21 | **Private / incognito window** | 🧱🟡 | P1 | None. All tabs share one ephemeral session — *accidentally* non-persistent, not *isolated*. | A real boundary needs per-session storage/cookies; verify libservo can give a webview an isolated, non-persisted storage scope. |
| 22 | **Session restore (reopen tabs)** | ❌ | P0 | None. Closing the window loses everything. | Persist tab list + per-tab URL (history is harder); restore on launch; "reopen closed tab". |
| 23 | **Crash restore** | 🟡 | P1 | `notify_crashed` exists in the trait but swerve **doesn't implement it** (only 5 delegate methods total). A content crash = blank tab, no recovery. | Implement `notify_crashed` → sad-tab UI + reload; periodic session snapshot for restore. |
| 24 | **Tabs: new/select/close** | ✅ | P0 | Done & verified (`main.rs:256-323`; Ctrl+T/W/Tab `main.rs:666-684`). | — |
| 25 | **Tabs: favicons** | 🟡 | P0 | Engine provides `notify_favicon_changed` + `WebView::favicon()` (`webview.rs:355`); swerve ignores both. Tabs are title-only. | Implement the callback, decode the `Image`, render in tab strip + omnibox. |
| 26 | **Tabs: pinning** | ❌ | P1 | None. | Per-tab pinned flag + reordering + render. |
| 27 | **Tabs: groups** | ❌ | P2 | None. | Grouping model + collapsible UI. |
| 28 | **Tabs: vertical tabs** | ❌ | P1 | Horizontal strip only (`chrome.css`). Since chrome is HTML, this is a CSS/layout mode — cheap relative to Chrome. **Natural differentiator.** | Layout variant + content-rect now comes from left edge, not just top (extend the `swerve:layout` handshake to report a rect, not a scalar `top`). |
| 29 | **Tabs: search / overflow / scroll** | ❌ | P1 | None; strip will overflow with many tabs. | Overflow scroll + a tab-search palette. |
| 30 | **Tabs: drag-reorder** | ❌ | P1 | Explicitly deferred (M4b). | Pointer drag in chrome JS + model reorder command. |
| 31 | **Tabs: hibernation / throttling** | 🟡 | P1 | `WebView::set_throttled` exists (`webview.rs:707`); unused. Background tabs run full-tilt. | Throttle/`hide()` background tabs; discard + restore-on-activate for memory. |
| 32 | **Settings UI** | ❌ | P0 | None. No prefs page, nothing to persist. | `swerve://settings` HTML page bound to the store; the chrome-is-HTML model makes this cheap. |
| 33 | **Site permissions: prompts** | ❌ | P0 | `request_permission` exists with `PermissionFeature` {Geolocation, Notifications, Push, Midi, Camera, Microphone, Speaker, DeviceInfo, BackgroundSync, Bluetooth, PersistentStorage, ScreenWakeLock, Gamepad} (`embedder/lib.rs:619`). swerve **doesn't implement it → all such requests hang/deny by default**. | Implement → prompt UI + remember decision in store. |
| 34 | **Site permissions: management UI** | ❌ | P1 | None. | Per-site settings page + page-info popover (the "lock icon" panel). |
| 35 | **Authentication (HTTP basic/proxy)** | ❌ | P0 | `request_authentication` exists (`webview_delegate.rs`); not implemented → **auth-protected sites just fail**. | Username/password dialog (HTML overlay) wired to `AuthenticationRequest::authenticate`. |
| 36 | **JS dialogs (alert/confirm/prompt)** | ❌ | P0 | `show_embedder_control(SimpleDialog)` delivers these (`webview_delegate.rs:340`); not implemented → **`alert()`/`confirm()`/`prompt()` are no-ops**. | Modal HTML dialogs; must be visually un-spoofable per the spec note in Servo. |
| 37 | **`<select>` / `<input type=file/color/date>` pickers** | ❌ | P0 | `show_embedder_control` delivers `SelectElement`/`FilePicker`/`ColorPicker`/`InputMethod`; none implemented → **native form controls don't open**. | Implement each control's UI + `submit`/`dismiss`. |
| 37b | **Context menu (right-click)** | ❌ | P0 | `EmbedderControl::ContextMenu` is delivered (`webview_delegate.rs:340`); not implemented → **right-click does nothing**. | Build the menu UI + handle items (open-in-new-tab, copy link, save image, inspect, back/forward/reload). |
| 38 | **Popups / `window.open` / target=_blank** | ❌ | P0 | `request_create_new` (`CreateNewWebViewRequest`) not implemented → **links/JS that open new windows silently fail**. | Build the new WebView as a new tab/window; keep the handle alive (Servo warns it's dropped otherwise). |
| 39 | **Zoom (Ctrl+/-/0, pinch)** | 🟡 | P0 | `WebView::set_page_zoom`/`page_zoom`/`adjust_pinch_zoom` exist (`webview.rs:650-680`); **unused** — no key bindings, no per-site memory. | Wire Ctrl+/Ctrl-/Ctrl0 + wheel-zoom; persist per-site zoom. |
| 40 | **Print** | 🧱❌ | P1 | None. No print path in libservo at this rev. | Likely route via "print to PDF" once a PDF/print path exists; engine-dependent. |
| 41 | **Screenshot / capture** | 🟡 | P1 | `WebView::take_screenshot` exists (`webview.rs:784`); unused. | Wire a capture command (visible / full-page) + save dialog. Low-effort win. |
| 42 | **Translation** | ❌ | P2 | None. | Detect language + a translation backend (self-hostable to stay off Google). Big. |
| 43 | **Accessibility (screen readers)** | 🟡 | P1 | Engine ships full AccessKit: `notify_accessibility_tree_update` + `WebView::set_accessibility_active` (`webview.rs:890,908`). swerve **never activates it** → screen readers see nothing; the *HTML chrome itself* also needs an AccessKit adapter. | Activate per-webview + wire an AccessKit adapter to the platform AT bridge. Real work but unusually well-supported by Servo. |
| 44 | **Keyboard shortcuts (full set)** | 🟡 | P1 | Minimal map (`main.rs:122-139`: printable + nav/editing); tab Ctrl+T/W/Tab only. No Ctrl+L, Ctrl+F, Ctrl+R, Ctrl+±, reopen-tab, etc. | Adapt servoshell `keyutils.rs`; a bindable shortcut table. |
| 45 | **i18n / localization** | ❌ | P2 | English-only hardcoded strings in chrome HTML. | String catalog + locale switch; affects all chrome pages. |
| 46 | **IME / composition input** | 🧱🟡 | P1 | `InputMethodControl` delivered via `show_embedder_control`; swerve sends only raw key events (`main.rs:664-697`), no composition → **CJK/dead-key input broken**. | Implement IME control + winit IME events. |
| 47 | **Default-browser handling** | ❌ | P1 | None. | Per-OS registration (xdg `.desktop`/mimeapps on Linux; registry on Win; LSSetDefault on mac) + a "set default" prompt. |
| 48 | **Protocol / deep-link handlers** | ❌ | P1 | `request_protocol_handler` exists (default-deny per Servo doc); not implemented → `registerProtocolHandler` no-ops. Also no OS-level `mailto:`/custom-scheme dispatch. | Implement the delegate + persist registrations + OS handler registration. |
| 49 | **`swerve://` internal pages framework** | ❌ | P0 | None. swerve uses `swerve:` only as a *command* channel (`request_navigation` intercept, `main.rs:494`); there's no internal-page *content* scheme. Internal pages currently load from `file://` (`content/home.html`). | A registered internal scheme (or a `load_web_resource` interceptor) serving settings/history/downloads/newtab. Foundational for #9/#11/#32/#34. |
| 50 | **New-tab page (real)** | 🟡 | P1 | Static `content/home.html` (search box is inert decoration; not wired to the omnibox). | Dynamic NTP: top sites (from history), bookmarks, themeable (ties to the Opera-GX goal). |
| 51 | **Status bar (hover URL) / loading UI** | 🟡 | P1 | `notify_status_text_changed` + `WebView::status_text()` + `notify_load_status_changed` exist; unused. No hover-URL, no progress/spinner. | Implement callbacks → status overlay + tab spinner. Cheap. |
| 52 | **Fullscreen (page-requested)** | 🟡 | P1 | `notify_fullscreen_state_changed` + `WebView::exit_fullscreen()` exist; unused → video fullscreen won't work right. | Implement: hide chrome, resize content to full window, Esc to exit. |
| 53 | **Clipboard (copy/paste/cut in content)** | 🟡 | P0 | `ClipboardDelegate` trait exists (`clipboard_delegate.rs`); `servo`'s default `clipboard` feature is on, but swerve sets no custom delegate — **verify copy/paste actually works in content**; chrome JS only blocks `selectstart` (`chrome.js:30`). | Confirm/implement a `ClipboardDelegate` bridging the OS clipboard. |
| 54 | **Notifications (web `Notification`)** | ❌ | P2 | `show_notification` (Web + Servo delegate) not implemented → notifications no-op. | OS notification bridge + permission (#33). |

**Counts:** ~54 line items. P0: ~22. Of those P0s, swerve **has** exactly one
(tabs), and ~6 more are "engine hook exists, just unimplemented" (favicons, dialogs,
form pickers, context menu, zoom, permissions, auth) — i.e. **cheap to close**. The
expensive P0s are downloads (#10/#11, engine-blocked), find-in-page (#18,
engine-blocked), session restore (#22), and the data store + internal-pages framework
(#14/#49) that everything else hangs off.

---

## Engine-blocked features — call these out loudly

These are the ones where "just build the UI" is **not** the plan, because libservo at
`ed1af70` lacks the primitive. They feed the project's #1 risk (the Servo sync
treadmill) and must be scoped before being promised:

| Feature | Why blocked | Options (least → most coupling to Servo) |
|---------|-------------|-------------------------------------------|
| **Downloads** (#10) | No download delegate or save-to-disk path in libservo (verified: nothing in `components/servo/` or `components/net/` beyond a `fetch` `Initiator::Download` enum value). | (a) Intercept attachment/`Content-Disposition` responses via `load_web_resource` + `WebResourceLoad`, buffer to disk yourself — works for simple cases, awkward for streaming/large files; (b) upstream a proper download API to Servo. |
| **Find-in-page** (#18) | No `find_*` on `WebView`. | (a) Inject a JS find-and-highlight overlay via `UserContentManager` — imperfect (no engine-native match navigation, fragile on complex DOMs); (b) upstream find to Servo (the clean answer). |
| **PDF viewing** (#19) | Servo has no PDF renderer. | Bundle a JS/WASM PDF viewer served via the internal scheme (#49) and auto-load it for `application/pdf`. Pure embedder work, but a whole subsystem. |
| **Print** (#40) | No print path. | Gate behind PDF/print-to-PDF; revisit after #19. |
| **Private/incognito isolation** (#21) | Need a verified per-webview isolated, non-persisted storage/cookie scope. | Investigate whether libservo can scope storage per webview/profile; otherwise run a separate engine instance with no `config_dir`. |
| **Passkeys/WebAuthn** (#17) | DOM/engine capability; presence in Servo unconfirmed. | Verify Servo `navigator.credentials`/WebAuthn support before scoping; may be a long pole. |
| **IME** (#46) | Hook exists (`InputMethodControl`) but the full composition loop (winit IME ↔ Servo) is unbuilt. | Implement embedder side; mostly embedder work but fiddly and platform-specific. |

External corroboration that the engine side stays a moving, incomplete target:
libservo is still officially **pre-alpha** as of mid-2026, with embedding capabilities
(RefreshDriver, preloaded resources, accessibility activation in v0.0.6) still
landing release-by-release — see Servo's blog and releases. Anything you build on a
not-yet-existing engine primitive inherits that churn.

---

## Recommended build order (what unblocks the most)

This dimension's dependency graph collapses to a clear front-loaded order:

1. **#14 Profile/data store (SQLite via `rusqlite` + per-OS data/config dir).** Also
   set Servo's `config_dir` so its **cookie jar finally persists**
   (`resource_thread.rs:213,672`). *Unblocks bookmarks, history, downloads, settings,
   permissions memory, session restore, password store, per-site zoom.* Nothing
   user-visible is durable until this exists.
2. **#49 Internal-pages framework** (`swerve://` content scheme via `load_web_resource`
   or a registered protocol). *Unblocks settings/history/downloads/permissions UIs and
   a real NTP* — and since chrome is already HTML, these pages are cheap once the
   plumbing exists.
3. **Close the "hook exists, unimplemented" P0 cluster** — these are mostly an
   afternoon each because Servo already hands you the request object: **context menu
   (#37b), JS dialogs (#36), form/file/color pickers (#37), permissions prompts (#33),
   HTTP auth (#35), popups/`window.open` (#38), favicons (#25), zoom keybindings
   (#39), status/loading UI (#51), fullscreen (#52), crash tab (#23).** This single
   batch flips the most P0 cells and removes the most "silently does nothing" bugs.
4. **Session restore (#22)** + reopen-closed-tab — directly on top of #14.
5. **History UI/search (#9) + bookmarks (#5–#7) + omnibox suggestions (#2/#3).** These
   are the "feels like a browser" tier and all sit on #14/#49.
6. **Downloads (#10/#11)** — start the engine investigation early (it's the riskiest
   P0); ship a basic interceptor-based path, plan to upstream a real one.
7. **Find-in-page (#18)** — decide JS-overlay vs. upstream now; it's a top-5 thing
   users try.
8. Then the P1/P2 tail: vertical tabs (#28, a natural differentiator given HTML
   chrome), accessibility activation (#43), default-browser (#47), IME (#46), profiles
   (#13), reader mode (#20), PDF (#19), translation (#42).

---

## Differentiators worth banking early (cheap because chrome is HTML)

- **Vertical tabs / tab layouts (#28)** and **deep theming** are *CSS problems* in
  swerve, not native-widget rewrites — this is the structural advantage over
  Chromium-based competitors and aligns with the Opera-GX-class goal. The
  `swerve:layout` handshake already reports the content top; extend it to a full rect
  so the chrome can claim a left/right strip.
- **Settings/NTP/history/downloads as HTML pages (#9/#11/#32/#50)** reuse the exact
  same render path as the chrome — low marginal cost once #49 lands.
- **Screenshot (#41)** and **status/loading UI (#51)** are near-free wins (engine
  methods already exist) that make the product feel finished.

---

## Open questions (need a decision or an engine investigation)

1. **Downloads:** intercept-and-buffer via `load_web_resource` now, or invest in
   upstreaming a Servo download API? (Affects #10–#12 and the maintenance budget.)
2. **Find-in-page:** ship a JS-overlay stopgap, or block on upstream? Users expect it
   at 1.0.
3. **Private browsing:** can a webview get an isolated, non-persisted storage/cookie
   scope within one engine instance, or does incognito require a second `Servo`
   instance with no `config_dir`? (Determines #21's whole architecture.)
4. **Passkeys/WebAuthn:** does Servo `ed1af70` implement `navigator.credentials` at
   all? If not, #17 is a multi-quarter engine item, not a feature.
5. **Clipboard (#53):** does content copy/paste work today with the default `clipboard`
   feature and no custom `ClipboardDelegate`, or is a bridge required? (Quick to test.)
6. **Lyku sync surface:** which of these stores (#14) sync, and at what granularity
   (bookmarks/history/passwords/settings/open-tabs)? This shapes the store schema, so
   decide before #14 is built, even if sync ships later.
7. **Password storage encryption:** OS keyring (libsecret/Keychain/DPAPI) vs. a
   master-password-derived key? Picks the dependency and the threat model for #15.

---

## Risks specific to product features

- **"Silent no-op" cliff.** Because ~35 delegate methods are unimplemented, large
  swaths of the live web *appear* to work but quietly fail (right-click, downloads,
  `confirm()`, `<select>`, `window.open`, auth prompts). This reads as "broken
  browser," not "early browser." The hook-exists P0 batch (build-order step 3) is the
  single highest-leverage fix.
- **Persistence-before-features inversion.** Building any feature UI before #14/#49
  means rework. Resist shipping in-memory bookmarks/history.
- **Engine-blocked P0s (downloads, find).** These are on the critical path to a
  credible 1.0 but are exactly where the Servo-sync maintenance cost concentrates —
  whatever you upstream, you then own across rev bumps.
- **Security-sensitive features (passwords #15, permissions #33, dialogs #36).** A
  half-built password manager or a spoofable JS dialog is worse than none; treat these
  as security-reviewed work, not feature checkboxes.
- **Scope creep via "Chrome parity."** Autofill (#16), translation (#42), passkeys
  (#17), and full i18n (#45) are each multi-quarter. Defer explicitly; don't let them
  block 1.0.
