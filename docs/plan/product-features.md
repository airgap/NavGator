# navgator — Browser Product Feature Inventory & Gap vs. M1–M5

> Scope: everything a full browser needs as a **product** (not engine internals),
> gapped against navgator's verified current state (repo at `/raid/NavGator`, Servo rev
> `ed1af70`). Each feature is tagged **have / partial / missing** with a target
> **priority** (P0 = required for a credible 1.0, P1 = expected by power users / fast
> follow, P2 = differentiator or later).
>
> **Architecture note (M6 native-chrome pivot):** the browser UI — toolbar, tabs, dialogs,
> menus, settings, find bar, bookmarks bar, omnibox dropdown — is now drawn **natively with
> egui** directly over the page. Servo renders **only web content** into an offscreen GL
> texture; egui blits that texture on its background layer and paints the chrome on top.
> There is no second Servo WebView for the UI and no `navgator:`/`chrome.js` string-bridge
> anymore; privileged actions are direct Rust calls. The old "UI is HTML rendered by Servo"
> model (chrome.js / index.html / a `navgator:` command channel) is gone.
>
> Ground truth used: `crates/navgator/src/main.rs` (~2375 lines; `struct AppState` impls
> `WebViewDelegate`), the `navgator_engine` facade (`crates/navgator-engine/src/lib.rs`),
> `crates/navgator/src/content/welcome.html`, and the cached Servo source at
> `/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70/`
> (`components/servo/webview_delegate.rs`, `webview.rs`, `servo_delegate.rs`,
> `components/shared/embedder/lib.rs`, `components/net/resource_thread.rs`,
> `ports/servoshell/desktop/dialog.rs`).

---

## TL;DR — the shape of the gap

navgator built a real **shell** — and after the M6 native-chrome pivot it now ships a
large slice of the day-to-day product layer on top of it: native egui chrome, tabs with
favicons and spinners, omnibox navigation **with a history-backed suggestion dropdown**,
JS dialogs, form/file/color pickers, a right-click context menu, permission prompts, HTTP
auth, a hover-URL status bar, find-in-page, zoom, a native settings panel, persisted
history + bookmarks (with a bookmarks bar), search-engine config, a `gator://`
internal-pages framework, a real new-tab/welcome page, and reopen-closed-tab. The earlier
revision of this doc marked most of those MISSING; they are now **done** (verified in
`main.rs`). What remains genuinely missing is the heavier tier — downloads, full session
restore, a history/downloads/permissions *manager UI*, crash recovery, profiles,
passwords, reader mode, PDF — and most of that still needs **engine work or substantial
embedder build-out**, because **libservo gives hooks, not features**.

Three structural facts still shape this dimension:

1. **A persistence layer now exists — as flat files, not a database.** navgator persists
   **settings** (`~/.config/navgator/settings.conf`, `load_settings`/`save_settings`),
   **history** and **bookmarks** as TSV (`history.tsv`/`bookmarks.tsv` via `config_file`,
   `save_history`, `save_bookmarks`, `record_visit`; `struct Profile` in `main.rs`). What's
   still missing is a *structured* store (SQLite) and a richer schema (downloads, passwords,
   per-site permissions/zoom, full sessions), plus **Servo's own `config_dir`** is still
   unset, so its `cookie_jar.json` remains ephemeral (`ServoBuilder::default()` in
   `resumed()`; `components/net/resource_thread.rs:213,672`). The TSV store unblocked
   history/bookmarks/suggestions; a real DB is the precondition for the rest of the list.

2. **libservo gives hooks, not features — and navgator now wires most of the P0 hooks.**
   At rev `ed1af70` the `WebViewDelegate` trait is rich, and `AppState` now **implements**
   a large chunk of it: `load_web_resource` (the `gator://` scheme), `notify_url_changed`,
   `notify_page_title_changed`, `notify_history_changed`, `notify_favicon_changed`
   (+`WebView::favicon()`), `notify_load_status_changed`, `notify_status_text_changed`,
   `request_create_new` (popups/`window.open`), `notify_closed`, `show_embedder_control`
   (simple dialogs + `<select>` + color + file pickers), `request_authentication`,
   `request_permission`, `notify_fullscreen_state_changed`, and `request_navigation`. The
   right-click menu is built natively (winit right-press → an egui context-menu overlay),
   not via `EmbedderControl::ContextMenu`. Still **unimplemented**: `notify_crashed`
   (no sad-tab/recovery), accessibility tree updates, IME composition
   (`InputMethodControl`), `request_protocol_handler`, and `show_notification`. So the old
   "silent no-op cliff" is largely closed for the common cases (`alert`/`confirm`/`prompt`,
   `<select>`, file/color pickers, auth, permissions, popups all work), but a content
   **crash** still blanks the tab and CJK/dead-key **IME** input is still broken.

3. **Some marquee features the engine does NOT provide; navgator builds them itself or
   does without.** **Find-in-page** is now shipped as a **JS-injected highlight overlay**
   (`FIND_JS` + a Ctrl+F egui bar with next/prev/count — `find_run`/`find_step`/`find_close`),
   since there's still no native `find_*` on `WebView`; it's the imperfect-but-usable
   option (#18 is now partial, not missing). Still genuinely expensive and mostly
   engine-blocked: **downloads** (no download delegate at all in libservo at this rev —
   only a `fetch` `Initiator::Download` enum value), **PDF viewing**, **reader mode**, and
   **a real private/incognito profile boundary**.

Of the "first five things a user tries," most now work: **right-click** opens a (small)
context menu, **find text** works (JS overlay), and **log in to a basic-auth site** prompts
properly. What still fails or no-ops: **downloading a file** (no engine download path),
**saved-password / autofill** login, and **full session restore** on relaunch (only
reopen-*closed-tab* exists). Those — plus downloads' manager UI and a history page — are
where the remaining priority is.

---

## Master feature table (prioritized)

Status legend: ✅ have · 🟡 partial · ❌ missing · 🧱 *engine-blocked* (libservo at
`ed1af70` lacks the primitive; needs Servo work or a heavy embedder workaround).

| # | Feature | Status | Priority | Current state in navgator | What's needed |
|---|---------|--------|----------|-------------------------|---------------|
| 1 | **Omnibox: URL parse + go** | 🟡 | P0 | `navigate_from_omnibox` (`main.rs`): `://` passthrough, dotted-no-space → `https://`, else search template. Heuristic-only. | Proper URL/term disambiguation, IDN/punycode, localhost/IP, bare `scheme:` passthrough. |
| 2 | **Omnibox: suggestions/autocomplete dropdown** | ✅ | P0 | Done. `suggestions()` matches history by URL/title and shows an egui dropdown under the address bar (`draw_chrome`, `main.rs`); click navigates. | Add inline autocomplete + bookmarks/search-suggest sources to the dropdown. |
| 3 | **Omnibox: history-weighted ranking** | 🟡 | P1 | Ranked by **visit count** only (`suggestions()` sorts by `HistoryEntry.visits`, top 6). No recency decay, no bookmark/host weighting. | Full frecency (visit count × recency decay), prefix/host match, include bookmarks. |
| 4 | **Omnibox: search-engine config + suggest API** | ✅ | P1 | Done. `SEARCH_ENGINES` (DDG default + Kagi/Bing/Google) with `%s` templates, chosen in the native Settings panel (`draw_settings`), plus a custom-URL field; persisted to `settings.conf`. | Keyword (`!bang`-style) search + an optional live suggest endpoint. |
| 5 | **Bookmarks: add/remove/star** | ✅ | P0 | Done. Ctrl+D toggles a bookmark for the active page (`toggle_bookmark_active`), persisted to `bookmarks.tsv`. No toolbar star icon (keyboard/bar only) and no dedicated manager page yet. | A star button in the toolbar + a `gator://bookmarks` manager. |
| 6 | **Bookmarks: folders / bookmarks bar** | 🟡 | P1 | **Bookmarks bar shipped** — a horizontal strip below the tabs when any bookmarks exist (`draw_chrome`), each button loads the page. No folders, no drag-org. | Folder tree model + nesting in the bar + drag-organise. |
| 7 | **Bookmarks: import/export (HTML/Netscape)** | ❌ | P1 | None. | Parser/serializer for the Netscape bookmark format (Chrome/FF interop). |
| 8 | **History: record visits** | ✅ | P0 | Done. `record_visit` (called from `notify_url_changed`/`notify_page_title_changed`) dedupes by URL, increments a visit count, caps at 2000, and persists to `history.tsv`; skips `about:`/`data:`/`file:`. | Add visit *timestamps* + transition types (TSV stores only url/title/visits today). |
| 9 | **History: UI + search** | ❌ | P0 | None. | A `navgator://history` page with text search + delete + clear-range. |
| 10 | **Downloads: trigger + save to disk** | 🧱❌ | P0 | None. A download today just fails silently — **libservo exposes no download delegate** at `ed1af70`. | Intercept attachment responses via `load_web_resource`/`WebResourceLoad` or add an engine download path; stream to disk. Hard. |
| 11 | **Downloads: manager UI** | ❌ | P0 | None. | `navgator://downloads` list, per-item progress, open/show-in-folder, cancel. |
| 12 | **Downloads: resume / pause** | 🧱❌ | P2 | None. | HTTP Range + persisted partial state. Depends on #10 path. |
| 13 | **Profiles / multi-account** | ❌ | P1 | None. Single ephemeral session. | Profile dirs (config_dir per profile), profile switcher, isolated cookie/storage. |
| 14 | **Profile/data store (the substrate)** | 🟡 | **P0** | **Flat-file store shipped:** settings (`settings.conf`) + history/bookmarks (TSV) under `$XDG_CONFIG_HOME/navgator` (`config_file`, `load_profile`, `save_*`). No DB, no migrations, no rows for downloads/passwords/permissions/sessions; Servo `config_dir` still unset. | Migrate to SQLite (`rusqlite`) with a real schema + migrations; set Servo's `config_dir` so cookies persist. |
| 15 | **Password manager (store/fill)** | ❌ | P1 | None. | Encrypted credential store (OS keyring), capture-on-submit, fill-on-load via injected script (`UserContentManager`). |
| 16 | **Autofill: forms / addresses / cards** | ❌ | P2 | None. | Profile data + field-heuristics + fill UI. Large surface. |
| 17 | **Passkeys / WebAuthn** | 🧱❌ | P2 | None. | WebAuthn is an engine/DOM capability; verify Servo support (likely absent) before promising. Engine-blocked. |
| 18 | **Find-in-page (Ctrl+F)** | 🧱🟡 | P0 | **Shipped as a JS overlay** (no native find API in libservo). Ctrl+F opens an egui find bar; `FIND_JS` wraps matches in `<span data-ngf>` via `evaluate_javascript`, `find_step` cycles ▲/▼ and scrolls, with a live count. Imperfect (regex-escaped substring, fragile on complex DOM, no engine match-nav). | Land native find in Servo for correctness; or harden the overlay. |
| 19 | **PDF viewing** | 🧱❌ | P1 | None. PDFs would download, not render. | Servo has no PDF renderer; bundle a WASM/JS viewer (e.g. pdf.js-equivalent) served via internal scheme, or native. |
| 20 | **Reader mode** | ❌ | P2 | None. | Article extraction (Readability-style) + a styled reader page; inject via internal scheme. Pure embedder work. |
| 21 | **Private / incognito window** | 🧱🟡 | P1 | None. All tabs share one ephemeral session — *accidentally* non-persistent, not *isolated*. | A real boundary needs per-session storage/cookies; verify libservo can give a webview an isolated, non-persisted storage scope. |
| 22 | **Session restore (reopen tabs)** | 🟡 | P0 | **Reopen-closed-tab shipped** (Ctrl+Shift+T → `reopen_closed_tab`, popping a per-session `closed_tabs` URL stack, also fed by close/close-others). No persisted session: relaunch still starts at the welcome page. | Persist the open-tab list + per-tab URL to the store; restore on launch. |
| 23 | **Crash restore** | ❌ | P1 | `notify_crashed` exists in the trait but navgator still **doesn't implement it**. A content crash = blank tab, no sad-tab, no recovery. | Implement `notify_crashed` → sad-tab UI + reload; periodic session snapshot for restore. |
| 24 | **Tabs: new/select/close** | ✅ | P0 | Done & verified (`new_tab`/`select_tab`/`close_tab`/`close_others`; `+` button, middle-click close, per-tab `×`, tab context menu; Ctrl+T/W/Tab/Shift+Tab + Ctrl+1–9 in the keyboard match). | — |
| 25 | **Tabs: favicons** | ✅ | P0 | Done. `notify_favicon_changed` decodes `WebView::favicon()` (all `PixelFormat`s → `favicon_color_image`), uploads to a GPU texture in `load_favicons`, and renders a 16px icon per tab (a `Spinner` while loading). | Also surface the favicon in the omnibox / suggestion rows. |
| 26 | **Tabs: pinning** | ❌ | P1 | None. | Per-tab pinned flag + reordering + render. |
| 27 | **Tabs: groups** | ❌ | P2 | None. | Grouping model + collapsible UI. |
| 28 | **Tabs: vertical tabs** | ❌ | P1 | Horizontal strip only (`chrome.css`). Since chrome is HTML, this is a CSS/layout mode — cheap relative to Chrome. **Natural differentiator.** | Layout variant + content-rect now comes from left edge, not just top (extend the `navgator:layout` handshake to report a rect, not a scalar `top`). |
| 29 | **Tabs: search / overflow / scroll** | ❌ | P1 | None; strip will overflow with many tabs. | Overflow scroll + a tab-search palette. |
| 30 | **Tabs: drag-reorder** | ❌ | P1 | Explicitly deferred (M4b). | Pointer drag in chrome JS + model reorder command. |
| 31 | **Tabs: hibernation / throttling** | 🟡 | P1 | `select_tab` already `hide()`s inactive webviews and `show()`s the active one, but `WebView::set_throttled` is unused, so background tabs still run full-tilt. | Throttle background tabs + discard/restore-on-activate for memory. |
| 32 | **Settings UI** | ✅ | P0 | Done — **as a native egui panel**, not a web page. The ☰ button opens `draw_settings`: search-engine combo + custom URL, accent color, dark-theme toggle, all persisted via `save_settings` and applied live (`apply_theme`). | Expand to more prefs (downloads dir, permissions, per-site data) as those stores land. |
| 33 | **Site permissions: prompts** | ✅ | P0 | Done. `request_permission` pushes a `Dialog::Permission` egui overlay showing the requested `feature()` with Allow/Deny (`draw_one_dialog`). Decision is **not remembered** across requests. | Remember per-site decisions in the store (#14) + a management UI (#34). |
| 34 | **Site permissions: management UI** | ❌ | P1 | None. | Per-site settings page + page-info popover (the "lock icon" panel). |
| 35 | **Authentication (HTTP basic/proxy)** | ✅ | P0 | Done. `request_authentication` pushes a `Dialog::Auth` egui overlay (username + masked password, proxy-aware message) wired to `AuthenticationRequest::authenticate`; Cancel drops the handle. | Optional: remember/save credentials (needs the password store, #15). |
| 36 | **JS dialogs (alert/confirm/prompt)** | ✅ | P0 | Done. `show_embedder_control(SimpleDialog)` → a centered `Dialog::Simple` egui window; alert (OK), confirm (OK/Cancel), prompt (text field + OK/Cancel) all wired to `confirm()`/`dismiss()`/`set_current_value`. | Harden against spoofing (origin label / non-dismissable focus) per Servo's note. |
| 37 | **`<select>` / `<input type=file/color/date>` pickers** | 🟡 | P0 | `<select>` (incl. optgroups), file picker (via `egui-file-dialog`, single/multiple + extension filters), and color picker are all implemented in `show_embedder_control` → egui overlays with select/submit/dismiss. **Date/time** pickers and **IME** are not handled (the `_ => {}` arm). | Add date/time control UI + IME (#46). |
| 37b | **Context menu (right-click)** | 🟡 | P0 | Done minimally — a **native** egui context menu (right-press over the page → `Dialog::ContextMenu` at the cursor) with Back / Forward / Reload. Built directly in the winit handler, not from `EmbedderControl::ContextMenu`. Missing the link/image items. | Add open-in-new-tab, copy/save link, save image, view source/inspect — needs the engine to surface the hit-test target. |
| 38 | **Popups / `window.open` / target=_blank** | ✅ | P0 | Done. `request_create_new` builds the requested WebView (`request.builder(...)`) and `adopt_tab`s it as a new tab, keeping the handle alive; `notify_closed` removes it. | Optional: real popup-window sizing / `noopener` policy. |
| 39 | **Zoom (Ctrl+/-/0, pinch)** | 🟡 | P0 | Done for the common path: Ctrl+`=`/`+`, Ctrl+`-`, Ctrl+`0`, and Ctrl+wheel call `zoom_in`/`zoom_out`/`zoom_reset` → `WebView::set_page_zoom`, clamped 0.3–3.0, stored per-tab. No per-site **persistence**; touchpad pinch not wired. | Persist per-site zoom in the store; wire `adjust_pinch_zoom`. |
| 40 | **Print** | 🧱❌ | P1 | None. No print path in libservo at this rev. | Likely route via "print to PDF" once a PDF/print path exists; engine-dependent. |
| 41 | **Screenshot / capture** | 🟡 | P1 | `WebView::take_screenshot` exists (`webview.rs:784`); unused. | Wire a capture command (visible / full-page) + save dialog. Low-effort win. |
| 42 | **Translation** | ❌ | P2 | None. | Detect language + a translation backend (self-hostable to stay off Google). Big. |
| 43 | **Accessibility (screen readers)** | 🟡 | P1 | Engine ships full AccessKit (`notify_accessibility_tree_update` + `WebView::set_accessibility_active`); navgator **never activates it** → screen readers see nothing in page content. (egui chrome has its own limited AccessKit surface.) | Activate accessibility per-webview + bridge the tree to the platform AT layer. |
| 44 | **Keyboard shortcuts (full set)** | 🟡 | P1 | Solid core, hardcoded in the winit handler: Ctrl+T/W/Shift+T (reopen), Ctrl+L (omnibox), Ctrl+R (reload), Ctrl+D (bookmark), Ctrl+F (find), Ctrl +/-/0 (zoom), Ctrl+1–9 (select tab), Ctrl+Tab/Shift+Tab (cycle), Esc (close find/dialog/fullscreen). No history nav (Alt+←/→), no devtools, not user-bindable. | A bindable shortcut table; fill the gaps (Alt+arrows, etc.). |
| 45 | **i18n / localization** | ❌ | P2 | English-only hardcoded strings in chrome HTML. | String catalog + locale switch; affects all chrome pages. |
| 46 | **IME / composition input** | 🧱🟡 | P1 | `InputMethodControl` is delivered via `show_embedder_control` but falls into the `_ => {}` arm; navgator forwards only raw mapped keys (`winit_key_to_servo`), no composition → **CJK/dead-key input broken**. | Implement the IME control + winit IME events. |
| 47 | **Default-browser handling** | ❌ | P1 | None. | Per-OS registration (xdg `.desktop`/mimeapps on Linux; registry on Win; LSSetDefault on mac) + a "set default" prompt. |
| 48 | **Protocol / deep-link handlers** | ❌ | P1 | `request_protocol_handler` exists (default-deny per Servo doc); not implemented → `registerProtocolHandler` no-ops. Also no OS-level `mailto:`/custom-scheme dispatch. | Implement the delegate + persist registrations + OS handler registration. |
| 49 | **`gator://` internal pages framework** | ✅ | P0 | Done. `load_web_resource` intercepts the `gator://` scheme and serves embedded HTML (`gator://welcome`/`newtab`/`home` → `render_gator_welcome`; unknown hosts → a 404 page) with proper `Content-Type` + `StatusCode::OK`. No `file://` dependency. Foundational for future `gator://history`/`downloads`/`bookmarks`. | Add the manager pages (history #9, downloads #11, etc.) on this same path. |
| 50 | **New-tab page (real)** | 🟡 | P1 | `gator://welcome` is a **dynamic** page (`render_gator_welcome`): a working search box that posts to the configured engine, accent-themed from settings, and bookmark quick-link tiles (first 12). Missing: top-sites-from-history, deeper theming. | Add top sites (from the history store) + richer theming (ties to the Opera-GX goal). |
| 51 | **Status bar (hover URL) / loading UI** | ✅ | P1 | Done. `notify_status_text_changed` drives a bottom-left status overlay (hover-URL / load status); `notify_load_status_changed` sets per-tab `loading`, rendering a `Spinner` in the tab strip and clearing stale status on a new load. | Optional: a determinate progress bar. |
| 52 | **Fullscreen (page-requested)** | ✅ | P1 | Done. `notify_fullscreen_state_changed` sets a `fullscreen` flag that makes `update` skip `draw_chrome` (toolbar height → 0, page fills the window); Esc calls `WebView::exit_fullscreen()`. | — |
| 53 | **Clipboard (copy/paste/cut in content)** | 🟡 | P0 | `ClipboardDelegate` trait exists (`clipboard_delegate.rs`); `servo`'s default `clipboard` feature is on, but navgator sets no custom delegate — **verify copy/paste actually works in content**; chrome JS only blocks `selectstart` (`chrome.js:30`). | Confirm/implement a `ClipboardDelegate` bridging the OS clipboard. |
| 54 | **Notifications (web `Notification`)** | ❌ | P2 | `show_notification` (Web + Servo delegate) not implemented → notifications no-op. | OS notification bridge + permission (#33). |

**Counts:** ~54 line items. P0: ~22. After the M6 native-chrome pivot, navgator now **has**
(✅) the bulk of the P0 cluster: tabs (#24), favicons (#25), omnibox suggestions (#2),
search config (#4), bookmarks add/remove (#5), history record (#8), JS dialogs (#36),
popups (#38), permissions (#33), auth (#35), settings UI (#32), the `gator://`
internal-pages framework (#49), status/loading UI (#51), and fullscreen (#52). **Partial**
(🟡) P0s now genuinely usable but incomplete: URL parse (#1), pickers (#37, no date/IME),
context menu (#37b, nav-only), zoom (#39, no per-site memory), find-in-page (#18, JS
overlay), session restore (#22, reopen-closed only), and the data store (#14, TSV not DB).
The **still-missing** P0s are downloads (#10/#11, engine-blocked) and a history/downloads
*manager UI* (#9/#11) — plus crash restore (#23). The cheap-hook batch is essentially
done; the remaining P0 cost is concentrated in downloads and the DB-backed store.

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
2. **#49 Internal-pages framework** (`navgator://` content scheme via `load_web_resource`
   or a registered protocol). *Unblocks settings/history/downloads/permissions UIs and
   a real NTP* — and since chrome is already HTML, these pages are cheap once the
   plumbing exists.
3. **The "hook exists, unimplemented" P0 cluster is now mostly DONE** — JS dialogs (#36),
   form/file/color pickers (#37), permissions (#33), HTTP auth (#35), popups (#38),
   favicons (#25), zoom (#39), status/loading UI (#51), fullscreen (#52), and a native
   context menu (#37b) all landed in the native-chrome build. The remaining stragglers
   from this batch are **crash tab (#23)** and **IME (#46)**; finish those + enrich the
   context menu (link/image items) and pickers (date/time).
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
  navgator, not native-widget rewrites — this is the structural advantage over
  Chromium-based competitors and aligns with the Opera-GX-class goal. The
  `navgator:layout` handshake already reports the content top; extend it to a full rect
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
