// swerve chrome behaviour.
//
// Right now this runs entirely inside the chrome webview and only logs intent.
// The chrome cannot yet *drive* the content webview, because that needs a bridge
// from this JS into the Rust embedder. Designing that bridge is Milestone 2.
//
// Planned bridge (see docs/ARCHITECTURE.md):
//   chrome JS  --(command)-->  embedder  --(libservo API)-->  content WebView
//   content WebView  --(title/url/load events)-->  embedder  --(event)-->  chrome JS
//
// Verso did this with ipc-channel + a small injected JS API. We'll likely expose a
// `window.swerve` object backed by a custom URL scheme or a postMessage channel the
// embedder intercepts via WebViewDelegate.

const swerve = {
  // Commands the chrome will eventually send to the embedder. No-ops for now.
  navigate(input) {
    console.log("[swerve] navigate:", input);
  },
  back() {
    console.log("[swerve] back");
  },
  forward() {
    console.log("[swerve] forward");
  },
  reload() {
    console.log("[swerve] reload");
  },
  newTab() {
    console.log("[swerve] newTab");
  },
};

// Expose for the eventual native bridge to hook / replace.
window.swerve = swerve;

// ── Non-selectable chrome ─────────────────────────────────────────────────────
// Servo's CSS `user-select` is an inert stub at this rev (parses but nothing
// honors it), so we can't get `user-select: none` from CSS. But Servo DOES fire a
// cancellable `selectstart`, so we suppress selection on the chrome ourselves and
// keep editable fields (the address bar) selectable.
document.addEventListener("selectstart", (e) => {
  if (e.target.closest("input, textarea, [contenteditable]")) return;
  e.preventDefault();
});

const $ = (id) => document.getElementById(id);

// ── Tab-title ellipsis ────────────────────────────────────────────────────────
// Servo doesn't implement `text-overflow: ellipsis` yet (it's gated behind the
// `layout.unimplemented` pref and rejected as an unknown property). So we truncate
// to fit the element's box ourselves and append a real ellipsis, binary-searching
// the widest prefix that fits. Reading scrollWidth forces layout, so each probe
// reflects the current font/width.
const ELLIPSIS = "…";

function truncateToFit(el) {
  const full = el.dataset.fullTitle ?? el.textContent;
  el.dataset.fullTitle = full;
  el.textContent = full;
  if (el.scrollWidth <= el.clientWidth) return; // fits as-is

  let lo = 0;
  let hi = full.length;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    el.textContent = full.slice(0, mid).trimEnd() + ELLIPSIS;
    if (el.scrollWidth <= el.clientWidth) lo = mid;
    else hi = mid - 1;
  }
  el.textContent = lo > 0 ? full.slice(0, lo).trimEnd() + ELLIPSIS : ELLIPSIS;
}

function setTabTitle(titleEl, title) {
  if (!titleEl) return;
  titleEl.dataset.fullTitle = title;
  truncateToFit(titleEl);
}

function retruncateAllTabs() {
  document.querySelectorAll(".tab-title").forEach(truncateToFit);
}

// Initial pass (script runs at end of <body>, so the tabs exist) and on resize,
// since available tab width changes with the window.
retruncateAllTabs();
window.addEventListener("resize", retruncateAllTabs);

$("omnibox").addEventListener("submit", (e) => {
  e.preventDefault();
  const raw = $("address").value.trim();
  if (!raw) return;
  // Bare query vs. URL heuristic — refine later.
  const looksLikeUrl = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw) || /\.[a-z]{2,}/i.test(raw);
  const target = looksLikeUrl
    ? raw.includes("://")
      ? raw
      : `https://${raw}`
    : `https://duckduckgo.com/?q=${encodeURIComponent(raw)}`;
  swerve.navigate(target);
});

$("back").addEventListener("click", () => swerve.back());
$("forward").addEventListener("click", () => swerve.forward());
$("reload").addEventListener("click", () => swerve.reload());
$("tab-new").addEventListener("click", () => swerve.newTab());

// The embedder will call these to push content-webview state into the chrome.
window.addEventListener("swerve:state", (e) => {
  const { url, title, canGoBack, canGoForward } = e.detail ?? {};
  if (typeof url === "string") $("address").value = url;
  if (typeof title === "string") {
    setTabTitle(document.querySelector(".tab.is-active .tab-title"), title || "New tab");
  }
  if (typeof canGoBack === "boolean") $("back").disabled = !canGoBack;
  if (typeof canGoForward === "boolean") $("forward").disabled = !canGoForward;
});
