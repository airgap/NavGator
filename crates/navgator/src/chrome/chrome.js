// navgator chrome behaviour.
//
// The chrome talks to the embedder by navigating to `navgator:` command URLs, which the
// Rust side intercepts in `request_navigation` (and denies, so the chrome stays put).
// The embedder pushes UI state back via the `navgator:state` CustomEvent (tab model,
// active URL, back/forward) and user settings via `navgator:settings`. Single-process
// bridge — to be replaced by a proper channel later.

const navgator = {
  navigate(input) { go("navgator:nav#" + input); },
  back() { go("navgator:back"); },
  forward() { go("navgator:forward"); },
  reload() { go("navgator:reload"); },
  newTab() { go("navgator:tab?new=1"); },
  selectTab(i) { go("navgator:tab?select=" + i); },
  closeTab(i) { go("navgator:tab?close=" + i); },
  openSettings() { go("navgator:settings"); },
  window(action) { go("navgator:window?action=" + action); },
};
window.navgator = navgator;

// Each command is a one-shot navigation the embedder denies.
function go(url) {
  window.location.href = url;
}

const $ = (id) => document.getElementById(id);
const ELLIPSIS = "…";

// Default search template until the engine pushes the configured one (see settings).
let searchTemplate = "https://duckduckgo.com/?q=%s";

// ── Non-selectable chrome ─────────────────────────────────────────────────────
// Servo's CSS `user-select` is an inert stub, but it fires cancellable `selectstart`.
document.addEventListener("selectstart", (e) => {
  if (e.target.closest("input, textarea, [contenteditable]")) return;
  e.preventDefault();
});

// ── Tab-title ellipsis (Servo lacks `text-overflow: ellipsis`) ────────────────
function truncateToFit(el) {
  const full = el.dataset.fullTitle ?? el.textContent;
  el.dataset.fullTitle = full;
  el.textContent = full;
  if (el.scrollWidth <= el.clientWidth) return;
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

// ── Tab strip, rendered from the engine's tab model ───────────────────────────
function renderTabs(tabs, active) {
  const strip = $("tabstrip");
  strip.innerHTML = "";
  tabs.forEach((tab, i) => {
    const el = document.createElement("div");
    el.className = "tab" + (i === active ? " is-active" : "");
    el.addEventListener("click", () => navgator.selectTab(i));

    const title = document.createElement("span");
    title.className = "tab-title";
    title.textContent = tab.title || "New tab";

    const close = document.createElement("button");
    close.className = "tab-close";
    close.setAttribute("aria-label", "Close tab");
    close.textContent = "×";
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      navgator.closeTab(i);
    });

    if (tab.loading) {
      const spin = document.createElement("span");
      spin.className = "tab-spin";
      spin.setAttribute("aria-label", "Loading");
      el.appendChild(spin);
    }
    el.appendChild(title);
    el.appendChild(close);
    strip.appendChild(el);
    truncateToFit(title);
  });

  const add = document.createElement("button");
  add.className = "tab-new";
  add.setAttribute("aria-label", "New tab");
  add.textContent = "+";
  add.addEventListener("click", () => navgator.newTab());
  strip.appendChild(add);
}

// ── State pushed from the engine ──────────────────────────────────────────────
window.addEventListener("navgator:state", (e) => {
  const d = e.detail ?? {};
  if (Array.isArray(d.tabs)) renderTabs(d.tabs, d.active ?? 0);
  // Don't clobber what the user is actively typing in the address bar.
  if (typeof d.url === "string" && document.activeElement !== $("address")) {
    $("address").value = d.url;
  }
  if (typeof d.canGoBack === "boolean") $("back").disabled = !d.canGoBack;
  if (typeof d.canGoForward === "boolean") $("forward").disabled = !d.canGoForward;
});

// ── Settings pushed from the engine ───────────────────────────────────────────
window.addEventListener("navgator:settings", (e) => {
  const d = e.detail ?? {};
  if (typeof d.search === "string" && d.search.includes("%s")) searchTemplate = d.search;
  if (typeof d.accent === "string") {
    document.documentElement.style.setProperty("--accent", d.accent);
  }
});

// ── Toolbar wiring (acts on the active tab) ───────────────────────────────────
$("omnibox").addEventListener("submit", (e) => {
  e.preventDefault();
  const raw = $("address").value.trim();
  if (!raw) return;
  const looksLikeUrl = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw) || /\.[a-z]{2,}/i.test(raw);
  const target = looksLikeUrl
    ? raw.includes("://")
      ? raw
      : "https://" + raw
    : searchTemplate.replace("%s", encodeURIComponent(raw));
  navgator.navigate(target);
});
$("back").addEventListener("click", () => navgator.back());
$("forward").addEventListener("click", () => navgator.forward());
$("reload").addEventListener("click", () => navgator.reload());
$("menu").addEventListener("click", () => navgator.openSettings());
// Select-all on focus so typing replaces the URL instead of appending.
$("address").addEventListener("focus", (e) => e.target.select());

// ── Window controls (OS decorations are disabled) ─────────────────────────────
$("win-min").addEventListener("click", () => navgator.window("minimize"));
$("win-max").addEventListener("click", () => navgator.window("maximize"));
$("win-close").addEventListener("click", () => navgator.window("close"));

// Drag the window from empty titlebar space; double-click toggles maximize.
const titlebar = $("titlebar");
const isInteractive = (t) => t.closest("button, input, .tab");
titlebar.addEventListener("mousedown", (e) => {
  if (e.button === 0 && !isInteractive(e.target)) navgator.window("drag");
});
titlebar.addEventListener("dblclick", (e) => {
  if (!isInteractive(e.target)) navgator.window("maximize");
});

// ── Layout reporting (tells the engine where the content region starts) ───────
// ── JS dialogs (alert/confirm/prompt) — drawn as a chrome modal over the page ──
const modal = $("modal");
const modalInput = $("modal-input");
const modalCancel = $("modal-cancel");

function closeDialog(action) {
  modal.style.display = "none";
  go(
    "navgator:dialog?action=" +
      action +
      "&value=" +
      encodeURIComponent(modalInput.value || ""),
  );
}

window.addEventListener("navgator:dialog", (e) => {
  const d = e.detail ?? {};
  $("modal-msg").textContent = d.message || "";
  const isPrompt = d.kind === "prompt";
  modalInput.value = d.value || "";
  modalInput.style.display = isPrompt ? "block" : "none";
  modalCancel.style.display = d.kind === "alert" ? "none" : "inline-block";
  modal.style.display = "flex";
  if (isPrompt) {
    modalInput.focus();
    modalInput.select();
  } else {
    $("modal-ok").focus();
  }
});

$("modal-ok").addEventListener("click", () => closeDialog("ok"));
modalCancel.addEventListener("click", () => closeDialog("cancel"));
modal.addEventListener("keydown", (e) => {
  if (e.key === "Enter") closeDialog("ok");
  else if (e.key === "Escape") closeDialog("cancel");
});

// ── Layout reporting (tells the engine where the content region starts) ───────
function contentTopCss() {
  return Math.round($("viewport").getBoundingClientRect().top);
}
window.addEventListener("resize", () => go("navgator:layout?top=" + contentTopCss()));

// Announce readiness + initial layout; the engine replies with the tab model + settings.
go("navgator:ready?top=" + contentTopCss());
