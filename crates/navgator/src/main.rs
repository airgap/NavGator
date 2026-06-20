//! navgator — a web browser with a native (egui) chrome and Servo as the page renderer.
//!
//! ## Native-chrome architecture (the M6 pivot)
//! The browser UI (toolbar, tabs, dialogs, menus) is drawn with **egui** directly over
//! the page, instead of being a second Servo WebView rendering HTML. Servo renders only
//! web content, into an `OffscreenRenderingContext`; each frame egui draws the page
//! texture on its background layer (via `render_to_parent_callback`) and the chrome
//! panels on top. This is how servoshell — Servo's own reference shell — is built.
//!
//! Why: the old "UI is HTML rendered by Servo" model needed a two-webview compositor and
//! a `navgator:` URL string-bridge, and made overlays (context menu, dialogs) painful.
//! Native chrome makes them trivial (an egui `Area`/`Window`), is leaner (no second engine
//! document), and gives a clean privilege boundary — privileged actions are direct Rust
//! calls, not URL messages parsed from a webview. Security + performance are the pitch.
//!
//! A `Weak<AppState>` self-reference lets `&self` delegate callbacks build new tab webviews
//! (which need the `Rc<AppState>` as their delegate).

use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::thread;

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{LayerId, PaintCallback};
use egui_file_dialog::{DialogState, FileDialog, Filter};
use egui_glow::{CallbackFn, EguiGlow};
use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
// Everything from the engine comes through navgator-engine, the only crate that touches
// the Servo fork (ROADMAP §R2; docs/FORK.md). IPC wire types come from navgator-protocol.
use navgator_engine::{
    AuthenticationRequest, ColorPicker, CreateNewWebViewRequest, DevicePoint, EmbedderControl,
    EmbedderControlId, EventLoopWaker, FilePicker, FilterPattern, Image, InputEvent, JSValue, Key,
    KeyState, KeyboardEvent, LoadStatus,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey as ServoNamedKey, NavigationRequest, OffscreenRenderingContext, PermissionRequest,
    PixelFormat, Preferences, RenderingContext,
    RgbColor, SelectElement, SelectElementOptionOrOptgroup, Servo, ServoBuilder, SimpleDialog,
    WebResourceLoad, WebResourceResponse, WebView, WebViewBuilder, WebViewDelegate, WheelDelta,
    WheelEvent, WheelMode, WindowRenderingContext,
};
// `http` types for building the WebResourceResponse served to gator:// internal pages.
use navgator_engine::http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE};
use navgator_protocol::IpcCommand;

mod sync;
mod password;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowId};

/// Width of the invisible window-edge band (logical px) that starts a resize on the
/// borderless window — OS decorations are off, so we hit-test it and `drag_resize_window`.
const RESIZE_BORDER: f64 = 6.0;

/// Page-zoom step + bounds (Ctrl +/-/0, Ctrl+wheel).
const ZOOM_STEP: f32 = 1.1;
const ZOOM_MIN: f32 = 0.3;
const ZOOM_MAX: f32 = 3.0;

fn main() -> Result<(), Box<dyn Error>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let event_loop = EventLoop::with_user_event().build()?;

    // Refresh the ad/tracker blocklists (EasyList/EasyPrivacy) in the background; the cached
    // copies take effect on the next launch.
    spawn_filter_update();

    let ipc_clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    if let Ok(path) = env::var("NAVGATOR_IPC") {
        start_ipc(path, event_loop.create_proxy(), ipc_clients.clone());
    }

    let mut app = App::Initial {
        waker: Waker(event_loop.create_proxy()),
        ipc_clients,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Built-in search engines offered in Settings; the first is the default. The welcome page
/// and the omnibox both substitute the query for `%s` in the selected template.
const SEARCH_ENGINES: &[(&str, &str)] = &[
    ("DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("Kagi", "https://kagi.com/search?q=%s"),
    ("Bing", "https://www.bing.com/search?q=%s"),
    ("Google", "https://www.google.com/search?q=%s"),
];

/// Built-in theme presets: (name, accent `#rrggbb`, dark). The chrome AND the gator:// pages
/// follow the selected theme.
const THEMES: &[(&str, &str, bool)] = &[
    ("Midnight", "#5b8cff", true),
    ("Synthwave", "#ff4fd8", true),
    ("Forest", "#3ecf8e", true),
    ("Ember", "#ff7a45", true),
    ("Grape", "#a875ff", true),
    ("Slate", "#8b95a7", true),
    ("Daylight", "#2f6bff", false),
];

/// NavGator's internal welcome / new-tab page, served from the `gator://` scheme by
/// `AppState::load_web_resource`. Works everywhere (no filesystem dependency), unlike a
/// `file://` home page.
const WELCOME_URL: &str = "gator://welcome";

fn content_url() -> Url {
    if let Some(arg) = env::args().nth(1) {
        if let Ok(url) = Url::parse(&arg) {
            return url;
        }
        eprintln!("navgator: '{arg}' is not a valid URL, loading the welcome page instead");
    }
    Url::parse(WELCOME_URL).expect("gator://welcome is a valid URL")
}

/// True when the user passed a parseable URL on the command line. When set, that single URL
/// takes precedence over any saved session at startup.
fn cli_url_given() -> bool {
    env::args().nth(1).is_some_and(|arg| Url::parse(&arg).is_ok())
}

/// Load the previously-saved session: the open tabs' URLs, one per line. Crash-safe — a
/// missing or malformed file simply yields no tabs (we fall back to the welcome page).
fn load_session() -> Vec<Url> {
    let mut urls = Vec::new();
    if let Some(text) = config_file("session.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(u) = Url::parse(line) {
                urls.push(u);
            }
        }
    }
    urls
}

/// User settings, persisted to a small `key=value` config file.
#[derive(Clone)]
struct Settings {
    /// Search URL template; `%s` is replaced with the URL-encoded query.
    search: String,
    /// UI accent color (any CSS-style `#rrggbb`).
    accent: String,
    /// Dark chrome theme (vs light).
    dark: bool,
    /// Lyku sync (early access): the `lyk_` API key (stored locally) + per-collection opt-ins.
    sync_api_key: String,
    sync_bookmarks: bool,
    sync_history: bool,
    sync_passwords: bool,
    /// Block ads + trackers (adblock-rust). On by default — it's the pitch.
    block_ads: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search: "https://duckduckgo.com/?q=%s".to_string(),
            accent: "#5b8cff".to_string(),
            dark: true,
            sync_api_key: String::new(),
            sync_bookmarks: false,
            sync_history: false,
            sync_passwords: false,
            block_ads: true,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("navgator").join("settings.conf"))
}

fn load_settings() -> Settings {
    let mut s = Settings::default();
    if let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "search" => s.search = v.trim().to_string(),
                    "accent" => s.accent = v.trim().to_string(),
                    "dark" => s.dark = v.trim() == "true",
                    "sync_api_key" => s.sync_api_key = v.trim().to_string(),
                    "sync_bookmarks" => s.sync_bookmarks = v.trim() == "true",
                    "sync_history" => s.sync_history = v.trim() == "true",
                    "sync_passwords" => s.sync_passwords = v.trim() == "true",
                    "block_ads" => s.block_ads = v.trim() == "true",
                    _ => {}
                }
            }
        }
    }
    s
}

fn save_settings(s: &Settings) {
    if let Some(path) = settings_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            &path,
            format!(
                "search={}\naccent={}\ndark={}\nsync_api_key={}\nsync_bookmarks={}\nsync_history={}\nsync_passwords={}\nblock_ads={}\n",
                s.search,
                s.accent,
                s.dark,
                s.sync_api_key,
                s.sync_bookmarks,
                s.sync_history,
                s.sync_passwords,
                s.block_ads
            ),
        );
    }
}

/// Load the per-collection Lyku-sync pull cursors (max `updated` last seen); 0 if unset.
fn load_sync_cursors() -> (i64, i64, i64) {
    let (mut b, mut h, mut p) = (0i64, 0i64, 0i64);
    if let Some(text) = config_file("sync-state.tsv").and_then(|p| std::fs::read_to_string(p).ok())
    {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "bookmarks" => b = v.trim().parse().unwrap_or(0),
                    "history" => h = v.trim().parse().unwrap_or(0),
                    "passwords" => p = v.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    (b, h, p)
}

fn save_sync_cursors(bookmarks: i64, history: i64, passwords: i64) {
    if let Some(path) = config_file("sync-state.tsv") {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(
            path,
            format!("bookmarks={bookmarks}\nhistory={history}\npasswords={passwords}\n"),
        );
    }
}

/// One visited page (frecency = visit count, for autocomplete ranking later).
struct HistoryEntry {
    url: String,
    title: String,
    visits: u32,
    /// Last-modified time (ms) for last-write-wins sync; 0 for rows from before sync existed.
    updated: i64,
}

struct Bookmark {
    url: String,
    title: String,
    /// Last-modified time (ms) for last-write-wins sync.
    updated: i64,
}

/// A download recorded for the gator://downloads manager. The engine streams the file to disk
/// (~/Downloads) and reports started/completed via the WebViewDelegate hooks.
struct Download {
    url: String,
    path: String,
    done: bool,
    success: bool,
}

/// Persisted browsing profile (history + bookmarks), stored as TSV under the config dir.
#[derive(Default)]
struct Profile {
    history: Vec<HistoryEntry>,
    bookmarks: Vec<Bookmark>,
}

fn config_file(name: &str) -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("navgator").join(name))
}

/// All adblock filter rules: the bundled starter list plus any cached EasyList / EasyPrivacy.
fn load_filter_rules() -> Vec<String> {
    let mut rules: Vec<String> = include_str!("content/blocklist.txt")
        .lines()
        .map(String::from)
        .collect();
    for name in ["easylist.txt", "easyprivacy.txt"] {
        if let Some(text) = config_file(name).and_then(|p| std::fs::read_to_string(p).ok()) {
            rules.extend(text.lines().map(String::from));
        }
    }
    rules
}

/// Refresh the cached EasyList / EasyPrivacy in the background (best-effort), re-fetching any
/// list that is missing or older than a week. The fresh lists take effect on the next launch.
fn spawn_filter_update() {
    std::thread::spawn(|| {
        const WEEK: u64 = 7 * 24 * 60 * 60;
        let lists = [
            ("easylist.txt", "https://easylist.to/easylist/easylist.txt"),
            (
                "easyprivacy.txt",
                "https://easylist.to/easylist/easyprivacy.txt",
            ),
        ];
        for (name, url) in lists {
            let Some(path) = config_file(name) else {
                continue;
            };
            let fresh = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() < WEEK)
                .unwrap_or(false);
            if fresh {
                continue;
            }
            if let Ok(resp) = ureq::get(url).call() {
                if let Ok(body) = resp.into_string() {
                    // sanity-check it's a real filter list, not an error/redirect page
                    if body.len() > 10_000 && body.contains("[Adblock") {
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(&path, body);
                    }
                }
            }
        }
    });
}

/// TSV cell sanitizer — fields can't contain the tab/newline separators.
fn tsv_field(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

/// Current time in milliseconds since the epoch — the per-item modification stamp used for
/// last-write-wins sync.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn load_profile() -> Profile {
    let mut p = Profile::default();
    if let Some(text) = config_file("history.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let mut it = line.splitn(4, '\t');
            if let (Some(u), Some(t), Some(v)) = (it.next(), it.next(), it.next()) {
                p.history.push(HistoryEntry {
                    url: u.to_string(),
                    title: t.to_string(),
                    visits: v.parse().unwrap_or(1),
                    updated: it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                });
            }
        }
    }
    if let Some(text) = config_file("bookmarks.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let mut it = line.splitn(3, '\t');
            if let (Some(u), Some(t)) = (it.next(), it.next()) {
                p.bookmarks.push(Bookmark {
                    url: u.to_string(),
                    title: t.to_string(),
                    updated: it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                });
            }
        }
    }
    p
}

fn save_history(p: &Profile) {
    if let Some(path) = config_file("history.tsv") {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let s: String = p
            .history
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}\n",
                    tsv_field(&e.url),
                    tsv_field(&e.title),
                    e.visits,
                    e.updated
                )
            })
            .collect();
        let _ = std::fs::write(path, s);
    }
}

fn save_bookmarks(p: &Profile) {
    if let Some(path) = config_file("bookmarks.tsv") {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let s: String = p
            .bookmarks
            .iter()
            .map(|b| format!("{}\t{}\t{}\n", tsv_field(&b.url), tsv_field(&b.title), b.updated))
            .collect();
        let _ = std::fs::write(path, s);
    }
}

/// History-backed omnibox suggestions: entries whose URL/title contains `query`,
/// ranked by frecency (visit count), top 6.
fn suggestions(history: &[HistoryEntry], query: &str) -> Vec<(String, String)> {
    let q = query.to_lowercase();
    let mut m: Vec<&HistoryEntry> = history
        .iter()
        .filter(|e| e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q))
        .collect();
    m.sort_by(|a, b| b.visits.cmp(&a.visits));
    m.into_iter()
        .take(6)
        .map(|e| (e.url.clone(), e.title.clone()))
        .collect()
}

/// Parse a `#rrggbb` accent into an egui color (Color32 has no hex constructor).
fn accent_color32(hex: &str) -> egui::Color32 {
    let s = hex.trim().trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return egui::Color32::from_rgb(r, g, b);
        }
    }
    egui::Color32::from_rgb(0x5b, 0x8c, 0xff)
}

/// Build the chrome's egui visuals from the user's accent + dark/light choice.
fn build_visuals(accent: egui::Color32, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let tint = |a: u8| egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), a);
    v.selection.bg_fill = tint(120);
    v.selection.stroke = egui::Stroke::new(1.0, accent);
    v.hyperlink_color = accent;
    v.text_cursor.stroke = egui::Stroke::new(2.0, accent);
    v.widgets.hovered.weak_bg_fill = tint(40);
    v.widgets.active.weak_bg_fill = tint(90);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    v
}

/// NavGator's web-feature profile: turn on high-value APIs Servo ships disabled by default
/// (see docs/plan/engine-gap.md — Servo's posture is "everything off"; NavGator's value-add
/// is a curated, distinct default). The first-wave items are common "silent breakers" of
/// modern sites and are low-risk API surfaces; IndexedDB/WebGL2 have real backends (rusqlite,
/// ANGLE) but should still be WPT-validated before being relied on. Accessibility and the
/// permission-prompted APIs (Geolocation/Notifications) need embedder plumbing and are
/// enabled in follow-ups.
fn navgator_preferences() -> Preferences {
    let mut p = Preferences::default();
    // Tier-0 first wave (engine-gap.md §15) — cheap, common, low-risk.
    p.dom_intersection_observer_enabled = true; // lazy-load / infinite scroll
    p.dom_adoptedstylesheet_enabled = true; // web components / Lit / Shadow DOM
    p.dom_fontface_enabled = true; // CSS Font Loading (web fonts)
    p.dom_web_animations_enabled = true; // Web Animations API
    p.dom_visual_viewport_enabled = true; // zoom/viewport-aware sites
    p.dom_async_clipboard_enabled = true; // navigator.clipboard
    // Permission-gated APIs — expose them; actual grants are prompted (request_permission).
    p.dom_permissions_enabled = true;
    p.dom_notification_enabled = true;
    p.dom_geolocation_enabled = true;
    // Tier-1 — real backends, high payoff (validate hardening before relying on them).
    p.dom_indexeddb_enabled = true; // rusqlite backend → web apps / PWAs
    p.dom_webgl2_enabled = true; // 3D / maps / games
    // Second wave — features with real implementations in the fork (additive, low-risk).
    p.dom_offscreen_canvas_enabled = true; // OffscreenCanvas (2d/bitmap/webgl)
    p.dom_sanitizer_enabled = true; // HTML Sanitizer API (security pitch)
    p.dom_exec_command_enabled = true; // contenteditable rich-text editing
    p.dom_storage_manager_api_enabled = true; // navigator.storage
    p
}

/// Percent-encode a search query for substitution into the `%s` of a search template.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Hex-encode bytes (for carrying the password ciphertext in a text sync payload).
fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Escape text for safe interpolation into HTML (the gator://welcome template).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Truncate a tab title to `max` chars with an ellipsis.
fn truncate_ellipsis(input: &str, max: usize) -> String {
    if input.chars().count() > max {
        let t: String = input.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    } else {
        input.to_string()
    }
}

/// Convert a decoded favicon (any `PixelFormat`) into an `egui::ColorImage`.
fn favicon_color_image(image: &Image) -> egui::ColorImage {
    let w = image.width as usize;
    let h = image.height as usize;
    match image.format {
        PixelFormat::K8 => egui::ColorImage::from_gray([w, h], image.data()),
        PixelFormat::KA8 => {
            let data: Vec<u8> = image
                .data()
                .chunks_exact(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([w, h], &data)
        }
        PixelFormat::RGB8 => egui::ColorImage::from_rgb([w, h], image.data()),
        PixelFormat::RGBA8 => egui::ColorImage::from_rgba_unmultiplied([w, h], image.data()),
        PixelFormat::BGRA8 => {
            let data: Vec<u8> = image
                .data()
                .chunks_exact(4)
                .flat_map(|c| [c[2], c[1], c[0], c[3]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([w, h], &data)
        }
    }
}

/// Parse a `#rrggbb` string into an `RgbColor`.
fn parse_hex_color(s: &str) -> Option<RgbColor> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    Some(RgbColor {
        red: u8::from_str_radix(&s[0..2], 16).ok()?,
        green: u8::from_str_radix(&s[2..4], 16).ok()?,
        blue: u8::from_str_radix(&s[4..6], 16).ok()?,
    })
}

/// The cursor icon for a resize-band direction.
fn resize_cursor(dir: ResizeDirection) -> CursorIcon {
    match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
    }
}

/// Escape a string into a JS double-quoted string literal.
/// Autofill JS: fill a login form's username + password. Called as `(AUTOFILL_JS)(u, p)`.
const AUTOFILL_JS: &str = r#"function(u,p){
  var pw=document.querySelector('input[type="password"]');
  if(!pw)return 0;
  var scope=pw.form||document;
  pw.value=p;
  var un=scope.querySelector('input[autocomplete="username"],input[type="email"],input[type="text"]');
  if(un)un.value=u;
  [un,pw].forEach(function(f){if(f){f.dispatchEvent(new Event('input',{bubbles:true}));f.dispatchEvent(new Event('change',{bubbles:true}));}});
  return 1;
}"#;

/// Read the active login form's username + password (for manual save). Returns JSON or "".
const READ_FORM_JS: &str = r#"(function(){
  var pw=document.querySelector('input[type="password"]');
  if(!pw||!pw.value)return "";
  var scope=pw.form||document;
  var un=scope.querySelector('input[autocomplete="username"],input[type="email"],input[type="text"]');
  return JSON.stringify({u:un?un.value:"",p:pw.value});
})()"#;

/// Collect the page's distinct class names + ids, so the cosmetic filter can decide which
/// generic element-hiding rules apply. Returns JSON `{c:[classes], i:[ids]}`.
const COSMETIC_COLLECT_JS: &str = r#"(function(){
  var c={},i={},els=document.querySelectorAll('[class],[id]');
  for(var k=0;k<els.length;k++){
    var e=els[k];
    if(e.id)i[e.id]=1;
    var cl=e.classList;
    if(cl)for(var j=0;j<cl.length;j++)c[cl[j]]=1;
  }
  return JSON.stringify({c:Object.keys(c),i:Object.keys(i)});
})()"#;

/// The origin (`scheme://host[:port]`) of a URL, for matching saved logins. None for non-web.
fn origin_of(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{}:{}", u.scheme(), host, p),
        None => format!("{}://{}", u.scheme(), host),
    })
}

fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Find-in-page highlighter (no native find API in the fork): wraps matches of `q` in
/// `<span data-ngf>` (first match orange, rest yellow), scrolls to the first, returns the
/// match count. Re-run on each query change; `find-step`/`find-clear` JS handle nav/cleanup.
const FIND_JS: &str = r#"function(q){
document.querySelectorAll('span[data-ngf]').forEach(function(s){var p=s.parentNode;if(p){p.replaceChild(document.createTextNode(s.textContent),s);p.normalize();}});
if(!q)return 0;
var rx;try{rx=new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g,'\\$&'),'gi');}catch(e){return 0;}
var w=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,null);
var nodes=[],n;while(n=w.nextNode()){var pn=n.parentNode;if(!pn)continue;if(/SCRIPT|STYLE|NOSCRIPT/.test(pn.nodeName))continue;rx.lastIndex=0;if(rx.test(n.nodeValue))nodes.push(n);}
var count=0;
nodes.forEach(function(node){var s=node.nodeValue,frag=document.createDocumentFragment(),last=0,m;rx.lastIndex=0;while(m=rx.exec(s)){if(m[0].length===0){rx.lastIndex++;continue;}if(m.index>last)frag.appendChild(document.createTextNode(s.slice(last,m.index)));var sp=document.createElement('span');sp.setAttribute('data-ngf','');sp.style.background=(count===0?'#ff9632':'#ffe45e');sp.style.color='#000';sp.textContent=m[0];frag.appendChild(sp);last=m.index+m[0].length;count++;}if(last<s.length)frag.appendChild(document.createTextNode(s.slice(last)));node.parentNode.replaceChild(frag,node);});
window.__ngfActive=0;
var f=document.querySelector('span[data-ngf]');if(f)f.scrollIntoView({block:'center'});
return count;
}"#;

/// Minimal winit→Servo key mapping (printable + editing/nav keys).
fn winit_key_to_servo(key: &WinitKey) -> Option<Key> {
    Some(match key {
        WinitKey::Character(s) => Key::Character(s.to_string()),
        WinitKey::Named(NamedKey::Space) => Key::Character(" ".to_string()),
        WinitKey::Named(NamedKey::Enter) => Key::Named(ServoNamedKey::Enter),
        WinitKey::Named(NamedKey::Backspace) => Key::Named(ServoNamedKey::Backspace),
        WinitKey::Named(NamedKey::Delete) => Key::Named(ServoNamedKey::Delete),
        WinitKey::Named(NamedKey::Tab) => Key::Named(ServoNamedKey::Tab),
        WinitKey::Named(NamedKey::Escape) => Key::Named(ServoNamedKey::Escape),
        WinitKey::Named(NamedKey::ArrowLeft) => Key::Named(ServoNamedKey::ArrowLeft),
        WinitKey::Named(NamedKey::ArrowRight) => Key::Named(ServoNamedKey::ArrowRight),
        WinitKey::Named(NamedKey::ArrowUp) => Key::Named(ServoNamedKey::ArrowUp),
        WinitKey::Named(NamedKey::ArrowDown) => Key::Named(ServoNamedKey::ArrowDown),
        WinitKey::Named(NamedKey::Home) => Key::Named(ServoNamedKey::Home),
        WinitKey::Named(NamedKey::End) => Key::Named(ServoNamedKey::End),
        _ => return None,
    })
}

/// One browser tab: a content webview plus the state egui mirrors into the chrome.
struct Tab {
    webview: WebView,
    url: String,
    title: String,
    can_back: bool,
    can_forward: bool,
    zoom: f32,
    loading: bool,
    /// Status text (hovered link URL / load status) for the bottom-left status bar.
    status_text: Option<String>,
    /// A decoded favicon awaiting upload to a GPU texture (uploaded during the egui frame,
    /// since `load_texture` needs the `egui::Context`).
    favicon_pending: Option<egui::ColorImage>,
    favicon_tex: Option<egui::TextureHandle>,
    /// Set when Servo reports this tab's renderer pipeline panicked; cleared on the next
    /// fresh load. While set, the tab is showing the `gator://crash` recovery page.
    crashed: bool,
    /// Pinned tabs sort ahead of the rest, render compact (favicon only), have no close
    /// button, and survive "close other tabs".
    pinned: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SimpleKind {
    Alert,
    Confirm,
    Prompt,
}

/// A flattened `<select>` option (a header has `id == None`).
struct SelectOpt {
    id: Option<usize>,
    label: String,
    disabled: bool,
}

/// A native (egui) overlay awaiting user input. The held engine request is consumed on
/// resolve; dropping it without resolving cancels (the engine's default).
enum Dialog {
    Simple {
        kind: SimpleKind,
        message: String,
        input: String,
        handle: Option<SimpleDialog>,
    },
    Auth {
        message: String,
        user: String,
        pass: String,
        handle: Option<AuthenticationRequest>,
    },
    Select {
        options: Vec<SelectOpt>,
        handle: Option<SelectElement>,
    },
    Color {
        hex: String,
        handle: Option<ColorPicker>,
    },
    File {
        dialog: FileDialog,
        handle: Option<FilePicker>,
    },
    Permission {
        message: String,
        handle: Option<PermissionRequest>,
    },
    ContextMenu {
        pos: egui::Pos2,
    },
}

struct AppState {
    servo: Servo,
    window_context: Rc<WindowRenderingContext>,
    content_context: Rc<OffscreenRenderingContext>,
    egui: RefCell<EguiGlow>,
    /// Height (logical px) of the egui chrome panels; the page begins below this.
    toolbar_height: Cell<f32>,
    /// The content webviews' current device-px size (page area), to avoid redundant resizes.
    content_px: Cell<(u32, u32)>,
    tabs: RefCell<Vec<Tab>>,
    active: Cell<usize>,
    /// Address-bar text + whether the user has edited it without navigating.
    location: RefCell<String>,
    location_dirty: Cell<bool>,
    /// Ctrl+L sets this; the next egui frame focuses + selects the address bar.
    focus_omnibox: Cell<bool>,
    /// Whether the native settings window is open.
    show_settings: Cell<bool>,
    /// Active native overlays (dialogs, pickers, context menu).
    dialogs: RefCell<Vec<Dialog>>,
    /// URLs of recently-closed tabs, for Ctrl+Shift+T (reopen most-recent).
    closed_tabs: RefCell<Vec<String>>,
    /// Find-in-page (Ctrl+F) state.
    find_open: Cell<bool>,
    find_query: RefCell<String>,
    find_matches: Cell<usize>,
    find_active: Cell<usize>,
    find_focus: Cell<bool>,
    fullscreen: Cell<bool>,
    scale: Cell<f64>,
    cursor: Cell<(f64, f64)>,
    ctrl: Cell<bool>,
    shift: Cell<bool>,
    weak_self: RefCell<Weak<AppState>>,
    ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    settings: RefCell<Settings>,
    /// Persisted history + bookmarks.
    profile: RefCell<Profile>,
    /// Lyku sync (early access): per-collection pull cursors, a status line, an in-flight guard.
    sync_cursor_bookmarks: Cell<i64>,
    sync_cursor_history: Cell<i64>,
    sync_cursor_passwords: Cell<i64>,
    /// Ad/tracker blocking engine (adblock-rust) + a session blocked counter.
    adblock: adblock::Engine,
    adblock_blocked: Cell<u64>,
    /// Cosmetic-filter CSS pending injection, deferred out of the eval callback to avoid
    /// re-entering Servo's JS evaluator. Each entry is `(webview, css)`.
    pending_cosmetic: RefCell<Vec<(WebView, String)>>,
    sync_status: RefCell<String>,
    syncing: Cell<bool>,
    /// Downloads (engine-streamed to ~/Downloads) + a transient toast for the latest one.
    downloads: RefCell<Vec<Download>>,
    download_toast: RefCell<Option<String>>,
    /// E2EE password store, the Settings passphrase input buffer, and a transient status line.
    password_store: RefCell<password::PasswordStore>,
    password_input: RefCell<String>,
    password_msg: RefCell<Option<String>>,
    event_proxy: EventLoopProxy<WakeUp>,
    /// Declared last so the GL contexts (which borrow the window) drop before it.
    window: Window,
}

impl Drop for AppState {
    fn drop(&mut self) {
        let _ = self.content_context.make_current();
        self.egui.borrow_mut().destroy();
    }
}

impl AppState {
    /// Render the `gator://welcome` page, templated with the current accent, the selected
    /// search engine, and the user's bookmarks (as quick-link tiles).
    /// Substitute the gator:// page theme color placeholders (`__BG__` … `__MUTED__`) for the
    /// current light/dark setting, so internal pages follow the chrome theme.
    fn themed(&self, html: String) -> Vec<u8> {
        let dark = self.settings.borrow().dark;
        let vars: [(&str, &str); 5] = if dark {
            [
                ("__BG__", "#0e1014"),
                ("__PANEL__", "#171a21"),
                ("__LINE__", "#262b36"),
                ("__FG__", "#e8eaed"),
                ("__MUTED__", "#9aa0aa"),
            ]
        } else {
            [
                ("__BG__", "#f5f6f8"),
                ("__PANEL__", "#ffffff"),
                ("__LINE__", "#e2e5ea"),
                ("__FG__", "#1b1f27"),
                ("__MUTED__", "#6b7280"),
            ]
        };
        let mut html = html;
        for (k, v) in vars {
            html = html.replace(k, v);
        }
        html.into_bytes()
    }

    fn render_gator_welcome(&self) -> Vec<u8> {
        let (search, accent) = {
            let s = self.settings.borrow();
            (s.search.clone(), s.accent.clone())
        };
        let engine = SEARCH_ENGINES
            .iter()
            .find(|(_, t)| *t == search)
            .map(|(n, _)| *n)
            .unwrap_or("the web");
        let bookmarks = {
            let p = self.profile.borrow();
            if p.bookmarks.is_empty() {
                "<p class=\"empty\">Bookmark a page with Ctrl+D and it will show up here.</p>"
                    .to_string()
            } else {
                let tiles: String = p
                    .bookmarks
                    .iter()
                    .take(12)
                    .map(|b| {
                        let title = if b.title.trim().is_empty() {
                            b.url.as_str()
                        } else {
                            b.title.as_str()
                        };
                        let letter = title
                            .chars()
                            .find(|c| c.is_alphanumeric())
                            .map(|c| c.to_uppercase().to_string())
                            .unwrap_or_else(|| "•".to_string());
                        format!(
                            "<a class=\"tile\" href=\"{}\" title=\"{}\"><span class=\"dot\">{}</span>{}</a>",
                            html_escape(&b.url),
                            html_escape(title),
                            html_escape(&letter),
                            html_escape(&truncate_ellipsis(title, 18)),
                        )
                    })
                    .collect();
                format!("<div class=\"links\">{tiles}</div>")
            }
        };
        let html = include_str!("content/welcome.html")
            .replace("__ACCENT__", &accent)
            .replace("__SEARCH_TEMPLATE__", &search)
            .replace("__SEARCH_ENGINE__", engine)
            .replace("__BOOKMARKS__", &bookmarks);
        self.themed(html)
    }

    /// Render the `gator://crash` recovery page for a tab whose renderer panicked. `url` is
    /// the address that was loaded when it crashed (the Reload button links back to it) and
    /// `reason` is Servo's panic message (shown under a Details disclosure).
    fn render_gator_crash(&self, url: &str, reason: &str) -> Vec<u8> {
        let accent = self.settings.borrow().accent.clone();
        let shown_url = if url.is_empty() { "about:blank" } else { url };
        let href = if url.is_empty() { WELCOME_URL } else { url };
        let reason = if reason.trim().is_empty() {
            "The renderer process exited unexpectedly."
        } else {
            reason
        };
        let html = include_str!("content/crash.html")
            .replace("__ACCENT__", &accent)
            .replace("__CRASH_HREF__", &html_escape(href))
            .replace("__CRASH_URL__", &html_escape(shown_url))
            .replace("__CRASH_REASON__", &html_escape(reason));
        self.themed(html)
    }

    /// Render the `gator://downloads` page: the session's downloads, newest first.
    fn render_gator_downloads(&self) -> Vec<u8> {
        let accent = self.settings.borrow().accent.clone();
        let rows = {
            let dl = self.downloads.borrow();
            if dl.is_empty() {
                "<p class=\"empty\">No downloads yet. Files you download are saved to ~/Downloads.</p>".to_string()
            } else {
                let mut out = String::new();
                for d in dl.iter().rev() {
                    let name = d.path.rsplit('/').next().unwrap_or(&d.path);
                    let (cls, label) = if !d.done {
                        ("run", "downloading…")
                    } else if d.success {
                        ("ok", "done")
                    } else {
                        ("err", "failed")
                    };
                    let letter = name
                        .chars()
                        .find(|c| c.is_alphanumeric())
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "•".to_string());
                    out.push_str(&format!(
                        "<div class=\"row\"><span class=\"ico\">{}</span><div class=\"meta\">\
                         <div class=\"name\">{}</div><div class=\"path\">{}</div></div>\
                         <span class=\"state {}\">{}</span></div>",
                        html_escape(&letter),
                        html_escape(name),
                        html_escape(&d.path),
                        cls,
                        label,
                    ));
                }
                format!("<div class=\"list\">{out}</div>")
            }
        };
        let html = include_str!("content/downloads.html")
            .replace("__ACCENT__", &accent)
            .replace("__ROWS__", &rows);
        self.themed(html)
    }

    /// Render `gator://passwords`: the saved-login manager. Passwords are masked (autofill uses
    /// them; they're never shown); each row has a Remove link (`?remove=<idx>`). Requires the
    /// store unlocked to list anything.
    fn render_gator_passwords(&self, remove: Option<usize>) -> Vec<u8> {
        if let Some(idx) = remove {
            let key = self
                .password_store
                .borrow()
                .all()
                .get(idx)
                .map(|c| (c.origin.clone(), c.username.clone()));
            if let Some((origin, username)) = key {
                self.password_store.borrow_mut().remove(&origin, &username);
                let _ = self.password_store.borrow().save();
            }
        }
        let accent = self.settings.borrow().accent.clone();
        let store = self.password_store.borrow();
        let rows = if !store.is_unlocked() {
            "<p class=\"empty\">The password store is locked. Unlock it in <strong>Settings → Passwords</strong>.</p>".to_string()
        } else if store.is_empty() {
            "<p class=\"empty\">No saved logins yet. On a login page, click 🔑 in the toolbar to save one.</p>".to_string()
        } else {
            let mut out = String::new();
            for (i, c) in store.all().iter().enumerate() {
                let host = c.origin.split("://").nth(1).unwrap_or(&c.origin);
                let letter = host
                    .chars()
                    .find(|ch| ch.is_alphanumeric())
                    .map(|ch| ch.to_string())
                    .unwrap_or_else(|| "•".to_string());
                out.push_str(&format!(
                    "<div class=\"row\"><span class=\"ico\">{}</span><div class=\"meta\">\
                     <div class=\"name\">{}</div><div class=\"path\">{} · ••••••••</div></div>\
                     <a class=\"rm\" href=\"gator://passwords?remove={}\">Remove</a></div>",
                    html_escape(&letter),
                    html_escape(host),
                    html_escape(&c.username),
                    i,
                ));
            }
            format!("<div class=\"list\">{out}</div>")
        };
        let html = include_str!("content/passwords.html")
            .replace("__ACCENT__", &accent)
            .replace("__ROWS__", &rows);
        self.themed(html)
    }

    /// Render the `gator://history` page: recent visits, newest-first, deduped by URL,
    /// each a clickable link showing title + url. Templated like `gator://welcome`.
    fn render_gator_history(&self) -> Vec<u8> {
        let accent = self.settings.borrow().accent.clone();
        let rows = {
            let p = self.profile.borrow();
            // `history` is append-on-first-visit order; show newest first and keep only the
            // first (most recent) occurrence of each URL.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut out = String::new();
            for e in p.history.iter().rev() {
                // record_visit already skips gator://about:/data:/file:; filter here too as
                // belt-and-suspenders so the history page never lists internal pages.
                if e.url.starts_with("gator:")
                    || e.url.starts_with("about:")
                    || e.url.starts_with("data:")
                    || e.url.starts_with("file:")
                {
                    continue;
                }
                if !seen.insert(e.url.as_str()) {
                    continue;
                }
                let title = if e.title.trim().is_empty() {
                    e.url.as_str()
                } else {
                    e.title.as_str()
                };
                out.push_str(&format!(
                    "<a class=\"row\" href=\"{}\"><span class=\"t\">{}</span><span class=\"u\">{}</span></a>",
                    html_escape(&e.url),
                    html_escape(&truncate_ellipsis(title, 80)),
                    html_escape(&e.url),
                ));
            }
            if out.is_empty() {
                "<p class=\"empty\">No history yet. Pages you visit will appear here.</p>".to_string()
            } else {
                format!("<div class=\"list\">{out}</div>")
            }
        };
        let html = include_str!("content/history.html")
            .replace("__ACCENT__", &accent)
            .replace("__ROWS__", &rows);
        self.themed(html)
    }

    /// Render the `gator://about` page: name, version, a one-line blurb, the keyboard
    /// shortcuts, and links back to welcome/history. Templated like `gator://welcome`.
    fn render_gator_about(&self) -> Vec<u8> {
        let accent = self.settings.borrow().accent.clone();
        let html = include_str!("content/about.html")
            .replace("__ACCENT__", &accent)
            .replace("__VERSION__", env!("CARGO_PKG_VERSION"));
        self.themed(html)
    }

    fn active_tab(&self) -> Option<WebView> {
        self.tabs
            .borrow()
            .get(self.active.get())
            .map(|t| t.webview.clone())
    }

    fn tab_index(&self, webview: &WebView) -> Option<usize> {
        self.tabs.borrow().iter().position(|t| &t.webview == webview)
    }

    fn active_nav(&self) -> (bool, bool) {
        self.tabs
            .borrow()
            .get(self.active.get())
            .map(|t| (t.can_back, t.can_forward))
            .unwrap_or((false, false))
    }

    /// If the cursor (physical px) sits in the window-edge resize band, the direction to
    /// resize; `None` when maximized or away from the edges.
    fn resize_direction_at(&self, x: f64, y: f64) -> Option<ResizeDirection> {
        if self.window.is_maximized() {
            return None;
        }
        let size = self.window.inner_size();
        let b = RESIZE_BORDER * self.scale.get();
        let (w, h) = (size.width as f64, size.height as f64);
        let (left, right, top, bottom) = (x <= b, x >= w - b, y <= b, y >= h - b);
        Some(match (top, bottom, left, right) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (true, _, _, true) => ResizeDirection::NorthEast,
            (_, true, true, _) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, _, _, _) => ResizeDirection::North,
            (_, true, _, _) => ResizeDirection::South,
            (_, _, true, _) => ResizeDirection::West,
            (_, _, _, true) => ResizeDirection::East,
            _ => return None,
        })
    }

    // ── egui frame build + paint ───────────────────────────────────────────────
    fn update(&self) {
        let _ = self.content_context.make_current();
        let mut egui = self.egui.borrow_mut();
        egui.run(&self.window, |ctx| {
            self.apply_theme(ctx);
            self.load_favicons(ctx);
            if !self.fullscreen.get() {
                self.draw_chrome(ctx);
            } else {
                self.toolbar_height.set(0.0);
            }
            self.draw_settings(ctx);
            self.draw_dialogs(ctx);

            // Status bar (hovered link URL / load status), bottom-left over the page.
            let status = self
                .tabs
                .borrow()
                .get(self.active.get())
                .and_then(|t| t.status_text.clone())
                .filter(|s| !s.is_empty());
            if let Some(status) = status {
                egui::Area::new(egui::Id::new("statusbar"))
                    .order(egui::Order::Foreground)
                    .interactable(false)
                    .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(status);
                        });
                    });
            }

            // Download toast (bottom-right) — click to open gator://downloads.
            let toast = self.download_toast.borrow().clone();
            if let Some(toast) = toast {
                let mut open_dl = false;
                egui::Area::new(egui::Id::new("download_toast"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Label::new(format!("↓  {toast}"))
                                            .sense(egui::Sense::click()),
                                    )
                                    .clicked()
                                {
                                    open_dl = true;
                                }
                                if ui.small_button("×").clicked() {
                                    *self.download_toast.borrow_mut() = None;
                                }
                            });
                        });
                    });
                if open_dl {
                    *self.download_toast.borrow_mut() = None;
                    if let (Ok(url), Some(tab)) =
                        (Url::parse("gator://downloads"), self.active_tab())
                    {
                        self.location_dirty.set(false);
                        tab.load(url);
                    }
                }
            }

            // Password action message (bottom-center), dismissible.
            let pmsg = self.password_msg.borrow().clone();
            if let Some(pmsg) = pmsg {
                egui::Area::new(egui::Id::new("password_msg"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -14.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(format!("🔑  {pmsg}")));
                                if ui.small_button("×").clicked() {
                                    *self.password_msg.borrow_mut() = None;
                                }
                            });
                        });
                    });
            }

            // Find-in-page bar (Ctrl+F), floating top-right under the chrome.
            if self.find_open.get() {
                egui::Area::new(egui::Id::new("findbar"))
                    .order(egui::Order::Foreground)
                    .anchor(
                        egui::Align2::RIGHT_TOP,
                        egui::vec2(-10.0, self.toolbar_height.get() + 8.0),
                    )
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let mut q = self.find_query.borrow_mut();
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut *q)
                                        .hint_text("Find in page")
                                        .desired_width(200.0)
                                        .id(egui::Id::new("find_input")),
                                );
                                if self.find_focus.take() {
                                    resp.request_focus();
                                }
                                let changed = resp.changed();
                                let query = q.clone();
                                drop(q);
                                if changed {
                                    self.find_run(&query);
                                }
                                ui.label(format!(
                                    "{}/{}",
                                    self.find_active.get(),
                                    self.find_matches.get()
                                ));
                                if ui.button("▲").clicked() {
                                    self.find_step(-1);
                                }
                                if ui.button("▼").clicked() {
                                    self.find_step(1);
                                }
                                if ui.button("✕").clicked() {
                                    self.find_close();
                                }
                            });
                        });
                    });
            }

            // The page occupies everything below the chrome panels. (At the Context
            // level egui's available_rect doesn't reflect panel reservations, so derive
            // the content rect from the toolbar height measured during draw_chrome.)
            let top = self.toolbar_height.get();
            let screen = ctx.content_rect();
            let avail = egui::Rect::from_min_max(egui::pos2(0.0, top), screen.max);
            let scale = ctx.pixels_per_point();
            let w = (avail.width() * scale).round().max(1.0) as u32;
            let h = (avail.height() * scale).round().max(1.0) as u32;
            if (w, h) != self.content_px.get() {
                self.content_px.set((w, h));
                self.content_context.resize(PhysicalSize::new(w, h));
                for t in self.tabs.borrow().iter() {
                    t.webview.resize(PhysicalSize::new(w, h));
                }
            }
            if let Some(tab) = self.active_tab() {
                tab.paint();
            }

            // Blit the page's offscreen FBO onto egui's background layer; chrome draws over it.
            if let Some(blit) = self.content_context.render_to_parent_callback() {
                ctx.layer_painter(LayerId::background()).add(PaintCallback {
                    rect: avail,
                    callback: Arc::new(CallbackFn::new(move |info, painter| {
                        let clip = info.viewport_in_pixels();
                        let target = Rect::new(
                            Point2D::new(clip.left_px, clip.from_bottom_px),
                            Size2D::new(clip.width_px, clip.height_px),
                        );
                        blit(painter.gl(), target);
                    })),
                });
            }
        });
        if egui.egui_ctx.has_requested_repaint() {
            self.window.request_redraw();
        }
    }

    fn paint(&self) {
        let _ = self.content_context.make_current();
        self.window_context.prepare_for_rendering();
        self.egui.borrow_mut().paint(&self.window);
        self.window_context.present();
    }

    /// Apply the user's accent + dark/light theme to the egui chrome each frame.
    fn apply_theme(&self, ctx: &egui::Context) {
        let (accent_hex, dark) = {
            let s = self.settings.borrow();
            (s.accent.clone(), s.dark)
        };
        let accent = accent_color32(&accent_hex);
        let theme = if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };
        ctx.set_theme(theme);
        ctx.set_visuals_of(theme, build_visuals(accent, dark));
    }

    /// Upload any decoded favicons to GPU textures (needs the egui Context, so done here).
    fn load_favicons(&self, ctx: &egui::Context) {
        for (i, tab) in self.tabs.borrow_mut().iter_mut().enumerate() {
            if let Some(img) = tab.favicon_pending.take() {
                tab.favicon_tex =
                    Some(ctx.load_texture(format!("favicon-{i}"), img, Default::default()));
            }
        }
    }

    /// Toolbar (nav + address + window controls) and the tab strip.
    fn draw_chrome(&self, ctx: &egui::Context) {
        let frame = egui::Frame::default()
            .fill(ctx.global_style().visuals.window_fill)
            .inner_margin(6.0);
        egui::TopBottomPanel::top("toolbar").frame(frame).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (cb, cf) = self.active_nav();
                if ui
                    .add_enabled(cb, egui::Button::new("◀").frame(false).min_size(egui::vec2(24.0, 24.0)))
                    .clicked()
                {
                    if let Some(t) = self.active_tab() {
                        t.go_back(1);
                    }
                }
                if ui
                    .add_enabled(cf, egui::Button::new("▶").frame(false).min_size(egui::vec2(24.0, 24.0)))
                    .clicked()
                {
                    if let Some(t) = self.active_tab() {
                        t.go_forward(1);
                    }
                }
                if ui
                    .add(egui::Button::new("↻").frame(false).min_size(egui::vec2(24.0, 24.0)))
                    .clicked()
                {
                    if let Some(t) = self.active_tab() {
                        t.reload();
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("✕").frame(false).min_size(egui::vec2(28.0, 24.0))).clicked() {
                        let _ = self.event_proxy.send_event(WakeUp::Exit);
                    }
                    if ui.add(egui::Button::new("▢").frame(false).min_size(egui::vec2(28.0, 24.0))).clicked() {
                        self.window.set_maximized(!self.window.is_maximized());
                    }
                    if ui.add(egui::Button::new("—").frame(false).min_size(egui::vec2(28.0, 24.0))).clicked() {
                        self.window.set_minimized(true);
                    }
                    if ui.add(egui::Button::new("☰").frame(false).min_size(egui::vec2(28.0, 24.0))).clicked() {
                        self.show_settings.set(!self.show_settings.get());
                    }
                    if self.password_store.borrow().is_unlocked()
                        && ui
                            .add(egui::Button::new("🔑").frame(false).min_size(egui::vec2(28.0, 24.0)))
                            .on_hover_text("Save this page's login")
                            .clicked()
                    {
                        self.save_login_active();
                    }
                    // Zoom indicator: only shown when the active tab isn't at 100%.
                    // Click resets the page zoom; in the right-to-left layout this sits
                    // just left of the menu button, at the right edge of the omnibox.
                    let zoom_pct = (self.active_zoom() * 100.0).round() as i32;
                    if zoom_pct != 100 {
                        let z = ui.add(
                            egui::Button::new(format!("{zoom_pct}%"))
                                .frame(false)
                                .min_size(egui::vec2(40.0, 24.0)),
                        );
                        if z.on_hover_text("Reset zoom (Ctrl+0)").clicked() {
                            self.zoom_reset();
                        }
                    }

                    let id = egui::Id::new("location_input");
                    let mut loc = self.location.borrow_mut();
                    let field = ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::singleline(&mut *loc)
                            .id(id)
                            .hint_text("Search or enter address"),
                    );
                    if field.changed() {
                        self.location_dirty.set(true);
                    }
                    if self.focus_omnibox.take() {
                        field.request_focus();
                    }
                    if field.gained_focus() {
                        if let Some(mut st) = TextEditState::load(ui.ctx(), id) {
                            st.cursor.set_char_range(Some(CCursorRange::two(
                                CCursor::new(0),
                                CCursor::new(loc.len()),
                            )));
                            st.store(ui.ctx(), id);
                        }
                    }
                    let mut go: Option<String> = None;
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        go = Some(loc.trim().to_string());
                    }
                    // History-backed autocomplete dropdown under the address bar.
                    if field.has_focus() && !loc.trim().is_empty() {
                        let sugg = suggestions(&self.profile.borrow().history, loc.trim());
                        if !sugg.is_empty() {
                            egui::Area::new(egui::Id::new("omnibox_suggest"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(field.rect.left_bottom())
                                .show(ui.ctx(), |ui| {
                                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                                        ui.set_min_width(field.rect.width().max(220.0));
                                        for (url, title) in &sugg {
                                            let label = if title.is_empty() {
                                                url.clone()
                                            } else {
                                                format!("{title}  —  {url}")
                                            };
                                            if ui
                                                .add(
                                                    egui::Button::new(truncate_ellipsis(&label, 80))
                                                        .frame(false),
                                                )
                                                .clicked()
                                            {
                                                go = Some(url.clone());
                                            }
                                        }
                                    });
                                });
                        }
                    }
                    if let Some(target) = go {
                        *loc = target.clone();
                        drop(loc);
                        self.location_dirty.set(false);
                        self.navigate_from_omnibox(&target);
                    }
                });
            });
        });

        let outer = egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            egui::ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let active = self.active.get();
                        // Pinned tabs sort ahead of the rest; `order` is render order, the
                        // underlying tab indices (used by select/close) are unchanged.
                        let order: Vec<usize> = {
                            let tabs = self.tabs.borrow();
                            let n = tabs.len();
                            let mut o: Vec<usize> = (0..n).filter(|&i| tabs[i].pinned).collect();
                            o.extend((0..n).filter(|&i| !tabs[i].pinned));
                            o
                        };
                        for &i in &order {
                            let (title, fav, loading, pinned) = {
                                let tabs = self.tabs.borrow();
                                let fav = tabs[i].favicon_tex.as_ref().map(|t| {
                                    egui::load::SizedTexture::new(t.id(), egui::vec2(16.0, 16.0))
                                });
                                (tabs[i].title.clone(), fav, tabs[i].loading, tabs[i].pinned)
                            };
                            let has_icon = loading || fav.is_some();
                            if loading {
                                ui.add(egui::Spinner::new().size(14.0));
                            } else if let Some(sized) = fav {
                                ui.add(
                                    egui::Image::from_texture(sized)
                                        .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                                );
                            }
                            // Pinned tabs are compact: the favicon is the identity, so the
                            // button carries no title (one glyph as a fallback if no icon).
                            let label = if pinned {
                                if has_icon {
                                    String::new()
                                } else {
                                    title.chars().next().map(|c| c.to_string()).unwrap_or_default()
                                }
                            } else {
                                truncate_ellipsis(&title, 20)
                            };
                            let tab = ui
                                .add(egui::Button::selectable(i == active, label))
                                .on_hover_text(&title);
                            if tab.clicked() && i != active {
                                self.select_tab(i);
                            }
                            if tab.middle_clicked() && !pinned {
                                self.close_tab(i);
                                break;
                            }
                            let mut menu_act = 0u8;
                            tab.context_menu(|ui| {
                                if ui.button("New tab").clicked() {
                                    menu_act = 1;
                                }
                                if ui
                                    .button(if pinned { "Unpin tab" } else { "Pin tab" })
                                    .clicked()
                                {
                                    menu_act = 4;
                                }
                                if ui.button("Close tab").clicked() {
                                    menu_act = 2;
                                }
                                if ui.button("Close other tabs").clicked() {
                                    menu_act = 3;
                                }
                            });
                            match menu_act {
                                1 => {
                                    self.new_tab(content_url());
                                    break;
                                }
                                2 => {
                                    self.close_tab(i);
                                    break;
                                }
                                3 => {
                                    self.close_others(i);
                                    break;
                                }
                                4 => {
                                    self.toggle_pin(i);
                                    break;
                                }
                                _ => {}
                            }
                            if !pinned
                                && ui.add(egui::Button::new("×").frame(false)).clicked()
                            {
                                self.close_tab(i);
                                break;
                            }
                        }
                        if ui.add(egui::Button::new("+").frame(false)).clicked() {
                            self.new_tab(content_url());
                        }
                    });
                });
        });
        let mut bottom = outer.response.rect.max.y;

        // Bookmarks bar (only when there are bookmarks), below the tab strip.
        let have_bookmarks = !self.profile.borrow().bookmarks.is_empty();
        if have_bookmarks {
            let bm = egui::TopBottomPanel::top("bookmarks").show(ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let bms: Vec<(String, String)> = self
                                .profile
                                .borrow()
                                .bookmarks
                                .iter()
                                .map(|b| (b.url.clone(), b.title.clone()))
                                .collect();
                            for (url, title) in bms {
                                let label = truncate_ellipsis(&title, 18);
                                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                                    if let (Ok(u), Some(tab)) = (Url::parse(&url), self.active_tab())
                                    {
                                        tab.load(u);
                                    }
                                }
                            }
                        });
                    });
            });
            bottom = bm.response.rect.max.y;
        }
        self.toolbar_height.set(bottom);
    }

    fn draw_settings(&self, ctx: &egui::Context) {
        if !self.show_settings.get() {
            return;
        }
        let mut open = true;
        let mut sync_clicked = false;
        let mut unlock_pass: Option<String> = None;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let mut s = self.settings.borrow_mut();
                let mut changed = false;
                ui.label("Search engine");
                let current = SEARCH_ENGINES
                    .iter()
                    .find(|(_, t)| *t == s.search)
                    .map(|(n, _)| *n)
                    .unwrap_or("Custom");
                egui::ComboBox::from_id_salt("search_engine")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (name, template) in SEARCH_ENGINES {
                            changed |= ui
                                .selectable_value(&mut s.search, template.to_string(), *name)
                                .changed();
                        }
                    });
                ui.add_space(4.0);
                ui.label("Custom search URL (use %s for the query)");
                changed |= ui.text_edit_singleline(&mut s.search).changed();
                ui.add_space(6.0);
                ui.label("Theme");
                let cur_theme = THEMES
                    .iter()
                    .find(|(_, a, d)| *a == s.accent && *d == s.dark)
                    .map(|(n, _, _)| *n)
                    .unwrap_or("Custom");
                egui::ComboBox::from_id_salt("theme_preset")
                    .selected_text(cur_theme)
                    .show_ui(ui, |ui| {
                        for (name, accent, dark) in THEMES {
                            if ui
                                .selectable_label(s.accent == *accent && s.dark == *dark, *name)
                                .clicked()
                            {
                                s.accent = accent.to_string();
                                s.dark = *dark;
                                changed = true;
                            }
                        }
                    });
                ui.add_space(6.0);
                ui.label("Accent color (#rrggbb)");
                changed |= ui.text_edit_singleline(&mut s.accent).changed();
                ui.add_space(6.0);
                changed |= ui.checkbox(&mut s.dark, "Dark theme").changed();

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Privacy").strong());
                changed |= ui
                    .checkbox(&mut s.block_ads, "Block ads & trackers")
                    .changed();
                ui.label(
                    egui::RichText::new(format!(
                        "{} ad/tracker request(s) blocked this session.",
                        self.adblock_blocked.get()
                    ))
                    .small()
                    .weak(),
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Lyku sync").strong());
                ui.label(
                    egui::RichText::new(
                        "Early access — syncs to your Lyku account. Expect rough edges.",
                    )
                    .small()
                    .color(egui::Color32::from_rgb(0xd6, 0x9a, 0x3c)),
                );
                ui.add_space(4.0);
                ui.label("Lyku API key");
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut s.sync_api_key)
                            .password(true)
                            .hint_text("lyk_…"),
                    )
                    .changed();
                ui.add_space(2.0);
                changed |= ui.checkbox(&mut s.sync_bookmarks, "Sync bookmarks").changed();
                changed |= ui.checkbox(&mut s.sync_history, "Sync history").changed();
                changed |= ui
                    .checkbox(&mut s.sync_passwords, "Sync passwords (E2EE — unlock below)")
                    .changed();
                if changed {
                    save_settings(&s);
                }
                ui.add_space(6.0);
                let busy = self.syncing.get();
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(if busy { "Syncing…" } else { "Sync now" }),
                    )
                    .clicked()
                {
                    sync_clicked = true;
                }
                let status = self.sync_status.borrow();
                if !status.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(status.as_str()).small().weak());
                }

                // Passwords (E2EE) — unlock the store to enable autofill + saving.
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Passwords (E2EE)").strong());
                if self.password_store.borrow().is_unlocked() {
                    ui.label(
                        egui::RichText::new(format!(
                            "Unlocked — {} saved. Use the 🔑 toolbar button to save the current page's login.",
                            self.password_store.borrow().len()
                        ))
                        .small()
                        .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Lock").clicked() {
                            self.password_store.borrow_mut().lock();
                            *self.password_msg.borrow_mut() = Some("Password store locked.".into());
                        }
                        if ui.button("Manage saved logins").clicked() {
                            if let (Ok(u), Some(tab)) =
                                (Url::parse("gator://passwords"), self.active_tab())
                            {
                                self.location_dirty.set(false);
                                tab.load(u);
                            }
                            self.show_settings.set(false);
                        }
                    });
                } else {
                    ui.label(
                        egui::RichText::new(
                            "Unlock with your sync passphrase to enable login autofill + saving.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut *self.password_input.borrow_mut())
                            .password(true)
                            .hint_text("sync passphrase"),
                    );
                    if ui.button("Unlock").clicked() {
                        unlock_pass = Some(self.password_input.borrow().clone());
                    }
                }
                if let Some(msg) = self.password_msg.borrow().clone() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(msg).small().weak());
                }
            });
        if let Some(p) = unlock_pass {
            self.unlock_passwords(&p);
            self.password_input.borrow_mut().clear();
        }
        if sync_clicked {
            self.start_sync();
        }
        if !open {
            self.show_settings.set(false);
        }
    }

    fn draw_dialogs(&self, ctx: &egui::Context) {
        let mut dialogs = self.dialogs.borrow_mut();
        let mut i = 0;
        while i < dialogs.len() {
            if self.draw_one_dialog(ctx, &mut dialogs[i]) {
                i += 1;
            } else {
                dialogs.remove(i);
            }
        }
    }

    /// Render one overlay; returns false when it has been resolved (and should be removed).
    fn draw_one_dialog(&self, ctx: &egui::Context, dialog: &mut Dialog) -> bool {
        let center = egui::Align2::CENTER_CENTER;
        match dialog {
            Dialog::Simple {
                kind,
                message,
                input,
                handle,
            } => {
                let mut keep = true;
                let title = match kind {
                    SimpleKind::Alert => "Alert",
                    SimpleKind::Confirm => "Confirm",
                    SimpleKind::Prompt => "Prompt",
                };
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(message.as_str());
                        if *kind == SimpleKind::Prompt {
                            ui.text_edit_singleline(input);
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                if let Some(h) = handle.take() {
                                    match h {
                                        SimpleDialog::Prompt(mut p) => {
                                            p.set_current_value(input);
                                            p.confirm();
                                        }
                                        other => other.confirm(),
                                    }
                                }
                                keep = false;
                            }
                            if *kind != SimpleKind::Alert && ui.button("Cancel").clicked() {
                                if let Some(h) = handle.take() {
                                    h.dismiss();
                                }
                                keep = false;
                            }
                        });
                    });
                keep
            }
            Dialog::Auth {
                message,
                user,
                pass,
                handle,
            } => {
                let mut keep = true;
                egui::Window::new("Authentication required")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(message.as_str());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("Username");
                            ui.text_edit_singleline(user);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Password");
                            ui.add(egui::TextEdit::singleline(pass).password(true));
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                if let Some(h) = handle.take() {
                                    h.authenticate(user.clone(), pass.clone());
                                }
                                keep = false;
                            }
                            if ui.button("Cancel").clicked() {
                                handle.take();
                                keep = false;
                            }
                        });
                    });
                keep
            }
            Dialog::Select { options, handle } => {
                let mut keep = true;
                egui::Window::new("Select")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            for opt in options.iter() {
                                let Some(id) = opt.id else {
                                    ui.label(egui::RichText::new(&opt.label).weak());
                                    continue;
                                };
                                if ui
                                    .add_enabled(!opt.disabled, egui::Button::new(&opt.label).frame(false))
                                    .clicked()
                                {
                                    if let Some(mut s) = handle.take() {
                                        s.select(vec![id]);
                                        s.submit();
                                    }
                                    keep = false;
                                }
                            }
                        });
                        ui.add_space(6.0);
                        if ui.button("Cancel").clicked() {
                            handle.take();
                            keep = false;
                        }
                    });
                keep
            }
            Dialog::Color { hex, handle } => {
                let mut keep = true;
                egui::Window::new("Choose a color")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Hex");
                            ui.text_edit_singleline(hex);
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                if let Some(mut p) = handle.take() {
                                    if let Some(rgb) = parse_hex_color(hex) {
                                        p.select(Some(rgb));
                                    }
                                    p.submit();
                                }
                                keep = false;
                            }
                            if ui.button("Cancel").clicked() {
                                handle.take();
                                keep = false;
                            }
                        });
                    });
                keep
            }
            Dialog::File { dialog, handle } => {
                enum Act {
                    Dismiss,
                    Submit,
                    Continue,
                }
                let act = if let Some(picker) = handle.as_mut() {
                    if *dialog.state() == DialogState::Closed {
                        if picker.allow_select_multiple() {
                            dialog.pick_multiple();
                        } else {
                            dialog.pick_file();
                        }
                    }
                    match dialog.update(ctx).state() {
                        DialogState::Open => Act::Continue,
                        DialogState::Picked(path) => {
                            picker.select(std::slice::from_ref(path));
                            Act::Submit
                        }
                        DialogState::PickedMultiple(paths) => {
                            picker.select(paths);
                            Act::Submit
                        }
                        _ => Act::Dismiss,
                    }
                } else {
                    Act::Dismiss
                };
                let keep = matches!(act, Act::Continue);
                match act {
                    Act::Dismiss => {
                        if let Some(p) = handle.take() {
                            p.dismiss();
                        }
                    }
                    Act::Submit => {
                        if let Some(p) = handle.take() {
                            p.submit();
                        }
                    }
                    Act::Continue => {}
                }
                keep
            }
            Dialog::Permission { message, handle } => {
                let mut keep = true;
                egui::Window::new("Permission request")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(message.as_str());
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Allow").clicked() {
                                if let Some(r) = handle.take() {
                                    r.allow();
                                }
                                keep = false;
                            }
                            if ui.button("Deny").clicked() {
                                if let Some(r) = handle.take() {
                                    r.deny();
                                }
                                keep = false;
                            }
                        });
                    });
                keep
            }
            Dialog::ContextMenu { pos } => {
                let mut keep = true;
                let r = egui::Area::new(egui::Id::new("context_menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(*pos)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(150.0);
                            let (cb, cf) = self.active_nav();
                            if ui.add_enabled(cb, egui::Button::new("Back").frame(false)).clicked() {
                                if let Some(t) = self.active_tab() {
                                    t.go_back(1);
                                }
                                keep = false;
                            }
                            if ui.add_enabled(cf, egui::Button::new("Forward").frame(false)).clicked() {
                                if let Some(t) = self.active_tab() {
                                    t.go_forward(1);
                                }
                                keep = false;
                            }
                            if ui.add(egui::Button::new("Reload").frame(false)).clicked() {
                                if let Some(t) = self.active_tab() {
                                    t.reload();
                                }
                                keep = false;
                            }
                        });
                    });
                if r.response.clicked_elsewhere() {
                    keep = false;
                }
                keep
            }
        }
    }

    fn push_dialog(&self, d: Dialog) {
        self.dialogs.borrow_mut().push(d);
        self.window.request_redraw();
    }

    fn navigate_from_omnibox(&self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let target = if raw.contains("://") {
            raw.to_string()
        } else if raw.contains('.') && !raw.contains(' ') {
            format!("https://{raw}")
        } else {
            self.settings.borrow().search.replace("%s", &url_encode(raw))
        };
        if let (Ok(url), Some(tab)) = (Url::parse(&target), self.active_tab()) {
            self.location_dirty.set(false);
            tab.load(url);
        }
    }

    // ── Page zoom ──────────────────────────────────────────────────────────────
    fn active_zoom(&self) -> f32 {
        self.tabs
            .borrow()
            .get(self.active.get())
            .map(|t| t.zoom)
            .unwrap_or(1.0)
    }

    fn apply_zoom(&self, zoom: f32) {
        let z = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if let Some(tab) = self.tabs.borrow_mut().get_mut(self.active.get()) {
            tab.webview.set_page_zoom(z);
            tab.zoom = z;
        }
    }

    fn zoom_in(&self) {
        self.apply_zoom(self.active_zoom() * ZOOM_STEP);
    }
    fn zoom_out(&self) {
        self.apply_zoom(self.active_zoom() / ZOOM_STEP);
    }
    fn zoom_reset(&self) {
        self.apply_zoom(1.0);
    }

    // ── Tab management ────────────────────────────────────────────────────────
    fn new_tab(&self, url: Url) {
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        let webview = WebViewBuilder::new(&self.servo, self.content_context.clone())
            .url(url)
            .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
            .delegate(me)
            .build();
        self.adopt_tab(webview);
    }

    fn adopt_tab(&self, webview: WebView) {
        let (w, h) = self.content_px.get();
        if w > 0 && h > 0 {
            webview.resize(PhysicalSize::new(w, h));
        }
        let idx = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(Tab {
                webview,
                url: String::new(),
                title: "New tab".to_string(),
                can_back: false,
                can_forward: false,
                zoom: 1.0,
                loading: false,
                status_text: None,
                favicon_pending: None,
                favicon_tex: None,
                crashed: false,
                pinned: false,
            });
            tabs.len() - 1
        };
        self.select_tab(idx);
        self.save_session();
    }

    /// Persist the open tabs' URLs so the next launch can restore them. Best-effort: write
    /// failures are ignored, matching save_history/save_bookmarks. Tabs whose URL hasn't been
    /// reported yet (still String::new()) are skipped.
    fn save_session(&self) {
        let Some(path) = config_file("session.tsv") else {
            return;
        };
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let s: String = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| !t.url.is_empty())
            .map(|t| format!("{}\n", tsv_field(&t.url)))
            .collect();
        let _ = std::fs::write(path, s);
    }

    fn select_tab(&self, i: usize) {
        {
            let tabs = self.tabs.borrow();
            if i >= tabs.len() {
                return;
            }
            for (j, tab) in tabs.iter().enumerate() {
                if j == i {
                    tab.webview.show();
                    tab.webview.set_throttled(false);
                } else {
                    // Background tabs are hidden AND throttled — less CPU/battery for tabs you
                    // aren't looking at (the anti-bloat pitch).
                    tab.webview.hide();
                    tab.webview.set_throttled(true);
                }
            }
            tabs[i].webview.focus();
            *self.location.borrow_mut() = tabs[i].url.clone();
        }
        self.location_dirty.set(false);
        self.active.set(i);
        self.window.request_redraw();
    }

    fn close_tab(&self, i: usize) {
        {
            let mut tabs = self.tabs.borrow_mut();
            if i >= tabs.len() {
                return;
            }
            let url = tabs[i].url.clone();
            if !url.is_empty() {
                self.closed_tabs.borrow_mut().push(url);
            }
            tabs.remove(i); // dropping the WebView handle closes the webview
        }
        if self.tabs.borrow().is_empty() {
            let _ = self.event_proxy.send_event(WakeUp::Exit);
            return;
        }
        let len = self.tabs.borrow().len();
        let active = self.active.get();
        let new_active = if active >= len {
            len - 1
        } else if active > i {
            active - 1
        } else {
            active
        };
        self.select_tab(new_active);
        self.save_session();
    }

    /// Close every tab except `keep` (tab context menu).
    /// Toggle the pinned state of tab `i`.
    fn toggle_pin(&self, i: usize) {
        if let Some(t) = self.tabs.borrow_mut().get_mut(i) {
            t.pinned = !t.pinned;
        }
        self.window.request_redraw();
    }

    fn close_others(&self, keep: usize) {
        {
            let tabs = self.tabs.borrow();
            if keep >= tabs.len() {
                return;
            }
            let mut closed = self.closed_tabs.borrow_mut();
            for (j, t) in tabs.iter().enumerate() {
                if j != keep && !t.pinned && !t.url.is_empty() {
                    closed.push(t.url.clone());
                }
            }
        }
        let new_active = {
            let mut tabs = self.tabs.borrow_mut();
            // Keep the target tab and any pinned tabs; drop the rest (order preserved).
            let keep_flags: Vec<bool> =
                (0..tabs.len()).map(|j| j == keep || tabs[j].pinned).collect();
            let new_active = keep_flags[..=keep].iter().filter(|&&k| k).count() - 1;
            let mut j = 0;
            tabs.retain(|_| {
                let k = keep_flags[j];
                j += 1;
                k
            });
            new_active
        };
        self.active.set(new_active);
        self.select_tab(new_active);
        self.save_session();
    }

    /// Reopen the most-recently-closed tab (Ctrl+Shift+T).
    fn reopen_closed_tab(&self) {
        let url = self.closed_tabs.borrow_mut().pop();
        if let Some(url) = url {
            if let Ok(u) = Url::parse(&url) {
                self.new_tab(u);
            }
        }
    }

    /// Record a page visit in history (deduped by URL; frecency = visit count).
    /// Kick off a background Lyku sync — push local bookmarks/history, pull remote changes.
    fn start_sync(&self) {
        if self.syncing.get() {
            return;
        }
        let (api_key, sync_bookmarks, sync_history, sync_passwords) = {
            let s = self.settings.borrow();
            (
                s.sync_api_key.clone(),
                s.sync_bookmarks,
                s.sync_history,
                s.sync_passwords,
            )
        };
        if api_key.trim().is_empty() {
            *self.sync_status.borrow_mut() = "Set a Lyku API key first.".into();
            return;
        }
        if !sync_bookmarks && !sync_history && !sync_passwords {
            *self.sync_status.borrow_mut() = "Enable a collection to sync first.".into();
            return;
        }
        let snap = {
            let p = self.profile.borrow();
            // Passwords are encrypted HERE (UI thread) — the sync thread only sees ciphertext,
            // and only when the store is unlocked.
            let store = self.password_store.borrow();
            let sync_passwords = sync_passwords && store.is_unlocked();
            let passwords = if sync_passwords {
                store
                    .all()
                    .iter()
                    .filter_map(|c| {
                        let blob = store.encrypt_credential(c)?;
                        Some((
                            format!("{}\u{1f}{}", c.origin, c.username),
                            hex_encode(&blob),
                            c.updated,
                        ))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            sync::SyncSnapshot {
                api_key,
                sync_bookmarks,
                sync_history,
                sync_passwords,
                passwords,
                bookmarks: p
                    .bookmarks
                    .iter()
                    .map(|b| (b.url.clone(), b.title.clone(), b.updated))
                    .collect(),
                history: p
                    .history
                    .iter()
                    .map(|e| (e.url.clone(), e.title.clone(), e.visits, e.updated))
                    .collect(),
                cursor_bookmarks: self.sync_cursor_bookmarks.get(),
                cursor_history: self.sync_cursor_history.get(),
                cursor_passwords: self.sync_cursor_passwords.get(),
            }
        };
        self.syncing.set(true);
        *self.sync_status.borrow_mut() = "Syncing…".into();
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            let outcome = sync::run_sync(snap);
            let _ = proxy.send_event(WakeUp::SyncDone(outcome));
        });
    }

    /// Apply a finished background sync to the local stores (UI thread). Last-write-wins by
    /// `updated`; deletes are not propagated in early access.
    fn apply_sync(&self, outcome: sync::SyncOutcome) {
        self.syncing.set(false);
        *self.sync_status.borrow_mut() = outcome.message.clone();
        if outcome.ok {
            {
                let mut p = self.profile.borrow_mut();
                for b in &outcome.bookmarks {
                    if b.deleted {
                        continue;
                    }
                    if let Some(local) = p.bookmarks.iter_mut().find(|x| x.url == b.url) {
                        if local.updated < b.updated {
                            local.title = b.title.clone();
                            local.updated = b.updated;
                        }
                    } else {
                        p.bookmarks.push(Bookmark {
                            url: b.url.clone(),
                            title: b.title.clone(),
                            updated: b.updated,
                        });
                    }
                }
                for h in &outcome.history {
                    if h.deleted {
                        continue;
                    }
                    if let Some(local) = p.history.iter_mut().find(|x| x.url == h.url) {
                        if local.updated < h.updated {
                            local.title = h.title.clone();
                            local.visits = local.visits.max(h.visits);
                            local.updated = h.updated;
                        }
                    } else {
                        p.history.push(HistoryEntry {
                            url: h.url.clone(),
                            title: h.title.clone(),
                            visits: h.visits,
                            updated: h.updated,
                        });
                    }
                }
                save_bookmarks(&p);
                save_history(&p);
            }
            // Decrypt + merge pulled passwords into the store (only while unlocked).
            if self.password_store.borrow().is_unlocked() {
                let mut changed = false;
                for pw in &outcome.passwords {
                    if pw.deleted {
                        continue;
                    }
                    if let Some(blob) = hex_decode(&pw.payload) {
                        let cred = self.password_store.borrow().decrypt_credential(&blob);
                        if let Some(cred) = cred {
                            self.password_store.borrow_mut().upsert(cred);
                            changed = true;
                        }
                    }
                }
                if changed {
                    let _ = self.password_store.borrow().save();
                }
            }
            self.sync_cursor_bookmarks.set(outcome.cursor_bookmarks);
            self.sync_cursor_history.set(outcome.cursor_history);
            self.sync_cursor_passwords.set(outcome.cursor_passwords);
            save_sync_cursors(
                outcome.cursor_bookmarks,
                outcome.cursor_history,
                outcome.cursor_passwords,
            );
        }
        self.window.request_redraw();
    }

    fn record_visit(&self, url: &str, title: &str) {
        if url.is_empty()
            || url.starts_with("gator:")
            || url.starts_with("about:")
            || url.starts_with("data:")
            || url.starts_with("file:")
        {
            return;
        }
        let mut p = self.profile.borrow_mut();
        if let Some(e) = p.history.iter_mut().find(|e| e.url == url) {
            e.visits += 1;
            if !title.is_empty() {
                e.title = title.to_string();
            }
            e.updated = now_ms();
        } else {
            p.history.push(HistoryEntry {
                url: url.to_string(),
                title: title.to_string(),
                visits: 1,
                updated: now_ms(),
            });
            const MAX_HISTORY: usize = 2000;
            if p.history.len() > MAX_HISTORY {
                let excess = p.history.len() - MAX_HISTORY;
                p.history.drain(0..excess);
            }
        }
        save_history(&p);
    }

    /// Bookmark or un-bookmark the active tab's page (Ctrl+D).
    fn toggle_bookmark_active(&self) {
        let (url, title) = match self.tabs.borrow().get(self.active.get()) {
            Some(t) => (t.url.clone(), t.title.clone()),
            None => return,
        };
        if url.is_empty() {
            return;
        }
        let mut p = self.profile.borrow_mut();
        if let Some(pos) = p.bookmarks.iter().position(|b| b.url == url) {
            p.bookmarks.remove(pos);
        } else {
            p.bookmarks.push(Bookmark {
                url,
                title,
                updated: now_ms(),
            });
        }
        save_bookmarks(&p);
    }

    /// Run the find highlighter for `query`; the async JS result updates find_matches.
    fn find_run(&self, query: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let js = format!("({FIND_JS})({})", js_string(query));
        let me = self.weak_self.borrow().clone();
        tab.evaluate_javascript(js, move |res| {
            if let Some(me) = me.upgrade() {
                if let Ok(JSValue::Number(n)) = res {
                    me.find_matches.set(n.max(0.0) as usize);
                    me.find_active.set(if n > 0.0 { 1 } else { 0 });
                    me.window.request_redraw();
                }
            }
        });
    }

    /// Move the active match forward (+1) or back (-1), scrolling it into view.
    fn find_step(&self, dir: i32) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let js = format!(
            "(function(d){{var ns=document.querySelectorAll('span[data-ngf]');if(!ns.length)return 0;var a=(window.__ngfActive||0)+d;if(a<0)a=ns.length-1;if(a>=ns.length)a=0;window.__ngfActive=a;ns.forEach(function(s,i){{s.style.background=(i===a?'#ff9632':'#ffe45e');}});ns[a].scrollIntoView({{block:'center'}});return a+1;}})({dir})"
        );
        let me = self.weak_self.borrow().clone();
        tab.evaluate_javascript(js, move |res| {
            if let Some(me) = me.upgrade() {
                if let Ok(JSValue::Number(n)) = res {
                    me.find_active.set(n as usize);
                    me.window.request_redraw();
                }
            }
        });
    }

    /// Close the find bar and remove all highlights.
    fn find_close(&self) {
        self.find_open.set(false);
        self.find_matches.set(0);
        if let Some(tab) = self.active_tab() {
            tab.evaluate_javascript(
                "document.querySelectorAll('span[data-ngf]').forEach(function(s){var p=s.parentNode;if(p){p.replaceChild(document.createTextNode(s.textContent),s);p.normalize();}});",
                |_| {},
            );
        }
        self.window.request_redraw();
    }

    /// Unlock the E2EE password store with the passphrase (decrypts saved logins into memory).
    fn unlock_passwords(&self, passphrase: &str) {
        let result = self.password_store.borrow_mut().unlock(passphrase);
        let msg = match result {
            Ok(()) => format!(
                "Password store unlocked ({} saved).",
                self.password_store.borrow().len()
            ),
            Err(e) => format!("Unlock failed: {e}"),
        };
        *self.password_msg.borrow_mut() = Some(msg);
        self.window.request_redraw();
    }

    /// Autofill the login form of tab `tab_idx` if the store is unlocked and a saved login
    /// matches the page origin. The credential goes straight from the store into the form via
    /// evaluate_javascript — it is never exposed to page-readable storage.
    fn autofill(&self, tab_idx: usize) {
        if !self.password_store.borrow().is_unlocked() {
            return;
        }
        let (webview, origin) = {
            let tabs = self.tabs.borrow();
            let Some(t) = tabs.get(tab_idx) else {
                return;
            };
            let Some(origin) = origin_of(&t.url) else {
                return;
            };
            (t.webview.clone(), origin)
        };
        let cred = self
            .password_store
            .borrow()
            .for_origin(&origin)
            .first()
            .map(|c| (c.username.clone(), c.password.clone()));
        let Some((user, pass)) = cred else {
            return;
        };
        let js = format!("({})({}, {})", AUTOFILL_JS, js_string(&user), js_string(&pass));
        webview.evaluate_javascript(js, |_| {});
    }

    /// Read the active page's login form and save it to the (unlocked) store.
    fn save_login_active(&self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let origin = self
            .tabs
            .borrow()
            .get(self.active.get())
            .and_then(|t| origin_of(&t.url));
        let Some(origin) = origin else {
            *self.password_msg.borrow_mut() = Some("Logins can only be saved on http(s) pages.".into());
            self.window.request_redraw();
            return;
        };
        if !self.password_store.borrow().is_unlocked() {
            *self.password_msg.borrow_mut() =
                Some("Unlock the password store first (Settings → Passwords).".into());
            self.window.request_redraw();
            return;
        }
        let me = self.weak_self.borrow().clone();
        tab.evaluate_javascript(READ_FORM_JS.to_string(), move |res| {
            let Some(me) = me.upgrade() else {
                return;
            };
            let Ok(JSValue::String(s)) = res else {
                return;
            };
            if s.is_empty() {
                *me.password_msg.borrow_mut() = Some("No filled password field on this page.".into());
                me.window.request_redraw();
                return;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                let user = v.get("u").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let pass = v.get("p").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !pass.is_empty() {
                    {
                        let mut store = me.password_store.borrow_mut();
                        store.upsert(password::Credential {
                            origin: origin.clone(),
                            username: user,
                            password: pass,
                            updated: now_ms(),
                        });
                        let _ = store.save();
                    }
                    *me.password_msg.borrow_mut() = Some(format!("Saved login for {origin}."));
                    me.window.request_redraw();
                }
            }
        });
    }

    /// Hide ad/clutter elements using EasyList's cosmetic (element-hiding) rules. Two evals:
    /// collect the page's class/id set, then inject a `<style>` hiding the matching selectors —
    /// generic rules are filtered to the page's actual classes/ids, so this stays cheap.
    fn apply_cosmetic(&self, tab_idx: usize) {
        if !self.settings.borrow().block_ads {
            return;
        }
        let (webview, url) = {
            let tabs = self.tabs.borrow();
            let Some(t) = tabs.get(tab_idx) else {
                return;
            };
            if origin_of(&t.url).is_none() {
                return; // http(s) pages only
            }
            (t.webview.clone(), t.url.clone())
        };
        let inject = webview.clone();
        let me = self.weak_self.borrow().clone();
        webview.evaluate_javascript(COSMETIC_COLLECT_JS.to_string(), move |res| {
            let Some(me) = me.upgrade() else {
                return;
            };
            let Ok(JSValue::String(s)) = res else {
                return;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
                return;
            };
            let arr = |k: &str| -> Vec<String> {
                v.get(k)
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default()
            };
            let (classes, ids) = (arr("c"), arr("i"));
            let cosmetic = me.adblock.url_cosmetic_resources(&url);
            let generic = me
                .adblock
                .hidden_class_id_selectors(&classes, &ids, &cosmetic.exceptions);
            let mut selectors: Vec<String> = cosmetic.hide_selectors.into_iter().collect();
            // generichide is ignored for v1: it is unreliable here (true even for localhost) and
            // the publisher opt-outs it guards are rare.
            selectors.extend(generic);
            if selectors.is_empty() {
                return;
            }
            let css = format!("{}{{display:none!important}}", selectors.join(","));
            // We're inside an eval callback (Servo's JS-evaluator RefCell is borrowed), so the
            // inject is deferred to the event loop via WakeUp::CosmeticReady.
            me.pending_cosmetic.borrow_mut().push((inject, css));
            let _ = me.event_proxy.send_event(WakeUp::CosmeticReady);
        });
    }

    /// Inject queued cosmetic-filter CSS. Called from the event loop (the JS evaluator is free
    /// there, unlike inside an eval callback). Each entry is `(webview, css)`.
    fn flush_cosmetic(&self) {
        let pending = std::mem::take(&mut *self.pending_cosmetic.borrow_mut());
        for (wv, css) in pending {
            let js = format!(
                "(function(c){{var s=document.getElementById('__ng_cos')||document.createElement('style');s.id='__ng_cos';s.textContent=c;(document.head||document.documentElement).appendChild(s);}})({})",
                js_string(&css)
            );
            wv.evaluate_javascript(js, |_| {});
        }
    }

    fn handle_ipc(&self, cmd: IpcCommand) {
        match cmd {
            IpcCommand::Navigate(url) => {
                if let (Ok(target), Some(tab)) = (Url::parse(&url), self.active_tab()) {
                    tab.load(target);
                }
            }
            IpcCommand::NewTab => self.new_tab(content_url()),
            IpcCommand::Reload => {
                if let Some(tab) = self.active_tab() {
                    tab.reload();
                }
            }
            IpcCommand::Back => {
                if let Some(tab) = self.active_tab() {
                    tab.go_back(1);
                }
            }
            IpcCommand::Forward => {
                if let Some(tab) = self.active_tab() {
                    tab.go_forward(1);
                }
            }
            IpcCommand::SelectTab(i) => self.select_tab(i),
            IpcCommand::CloseTab(i) => self.close_tab(i),
        }
    }

    fn ipc_emit(&self, line: &str) {
        let mut clients = self.ipc_clients.lock().unwrap();
        clients.retain_mut(|c| writeln!(c, "{line}").is_ok());
    }

    /// Forward a mouse/keyboard/wheel input to the active page webview.
    fn forward_to_page(&self, event: InputEvent) {
        if let Some(tab) = self.active_tab() {
            tab.notify_input_event(event);
        }
    }

    /// Device-px y at which the page area begins (below the chrome).
    fn toolbar_dev(&self) -> f64 {
        self.toolbar_height.get() as f64 * self.scale.get()
    }
}

impl WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    /// Serve NavGator's internal `gator://` pages (e.g. `gator://welcome`). Servo asks the
    /// embedder to intercept every resource load *before* it resolves the scheme, so a custom
    /// scheme works here with no engine fork patch and no net-internal ProtocolHandler. Loads
    /// we don't recognise are left alone (dropping `load` signals "do not intercept").
    fn load_web_resource(&self, webview: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        if url.scheme() != "gator" {
            // Ad/tracker blocking. This delegate already intercepts every load, so a matched
            // request is intercepted with an empty 204 instead of being fetched. `source` is the
            // requesting tab's URL, so first-vs-third-party rules resolve correctly.
            if matches!(url.scheme(), "http" | "https") && self.settings.borrow().block_ads {
                let source = self
                    .tab_index(&webview)
                    .and_then(|i| self.tabs.borrow().get(i).map(|t| t.url.clone()))
                    .unwrap_or_default();
                if let Ok(req) = adblock::request::Request::new(url.as_str(), &source, "other") {
                    if self.adblock.check_network_request(&req).matched {
                        self.adblock_blocked.set(self.adblock_blocked.get() + 1);
                        let response =
                            WebResourceResponse::new(url).status_code(StatusCode::NO_CONTENT);
                        let mut intercepted = load.intercept(response);
                        intercepted.finish();
                        return;
                    }
                }
            }
            return;
        }
        let body = match url.host_str().unwrap_or("welcome") {
            "welcome" | "newtab" | "home" => self.render_gator_welcome(),
            "history" => self.render_gator_history(),
            "about" => self.render_gator_about(),
            "downloads" => self.render_gator_downloads(),
            "passwords" => {
                let mut remove = None;
                for (k, v) in url.query_pairs() {
                    if k == "remove" {
                        remove = v.parse().ok();
                    }
                }
                self.render_gator_passwords(remove)
            }
            "crash" => {
                let mut crashed_url = String::new();
                let mut reason = String::new();
                for (k, v) in url.query_pairs() {
                    match k.as_ref() {
                        "url" => crashed_url = v.into_owned(),
                        "reason" => reason = v.into_owned(),
                        _ => {}
                    }
                }
                self.render_gator_crash(&crashed_url, &reason)
            }
            other => format!(
                "<!doctype html><meta charset=\"utf-8\">\
                 <body style=\"font-family:system-ui;background:#0e1014;color:#e8eaed;padding:48px\">\
                 <h1>gator://{}</h1><p>No such internal page. \
                 Try <a style=\"color:#5b8cff\" href=\"gator://welcome\">gator://welcome</a>.</p>",
                html_escape(other)
            )
            .into_bytes(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        let response = WebResourceResponse::new(url)
            .status_code(StatusCode::OK)
            .headers(headers);
        let mut intercepted = load.intercept(response);
        intercepted.send_body_data(body);
        intercepted.finish();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].url = url.to_string();
            self.ipc_emit(&format!("url {i} {url}"));
            if i == self.active.get() && !self.location_dirty.get() {
                *self.location.borrow_mut() = url.to_string();
            }
            let title = self.tabs.borrow()[i].title.clone();
            self.record_visit(url.as_str(), &title);
            self.save_session();
            self.window.request_redraw();
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if let Some(i) = self.tab_index(&webview) {
            let title = title.unwrap_or_else(|| "New tab".to_string());
            self.ipc_emit(&format!("title {i} {title}"));
            let url = self.tabs.borrow()[i].url.clone();
            self.tabs.borrow_mut()[i].title = title.clone();
            self.record_visit(&url, &title);
            self.window.request_redraw();
        }
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<Url>, current: usize) {
        if let Some(i) = self.tab_index(&webview) {
            let mut tabs = self.tabs.borrow_mut();
            tabs[i].can_back = current > 0;
            tabs[i].can_forward = current + 1 < entries.len();
            drop(tabs);
            self.window.request_redraw();
        }
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        if let Some(i) = self.tab_index(&webview) {
            let img = webview.favicon().map(|f| favicon_color_image(&f));
            self.tabs.borrow_mut()[i].favicon_pending = img;
            self.window.request_redraw();
        }
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if let Some(i) = self.tab_index(&webview) {
            {
                let mut tabs = self.tabs.borrow_mut();
                tabs[i].loading = !matches!(status, LoadStatus::Complete);
                // A new load clears any stale hover/status text and the crashed state
                // (a started load means the pipeline is alive again).
                if !matches!(status, LoadStatus::Complete) {
                    tabs[i].status_text = None;
                    tabs[i].crashed = false;
                }
            }
            if matches!(status, LoadStatus::Complete) {
                self.autofill(i);
                self.apply_cosmetic(i);
            }
            if matches!(status, LoadStatus::Complete) && i == self.active.get() {
                self.location_dirty.set(false);
            }
            self.window.request_redraw();
        }
    }

    fn notify_status_text_changed(&self, webview: WebView, status: Option<String>) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].status_text = status;
            self.window.request_redraw();
        }
    }

    fn notify_download_started(&self, _webview: WebView, url: String, path: String) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        *self.download_toast.borrow_mut() = Some(format!("Downloading {name}…"));
        self.downloads.borrow_mut().push(Download {
            url,
            path,
            done: false,
            success: false,
        });
        self.window.request_redraw();
    }

    fn notify_download_completed(&self, _webview: WebView, path: String, success: bool) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        *self.download_toast.borrow_mut() = Some(if success {
            format!("Saved {name}")
        } else {
            format!("Download failed: {name}")
        });
        if let Some(d) = self
            .downloads
            .borrow_mut()
            .iter_mut()
            .find(|d| d.path == path)
        {
            d.done = true;
            d.success = success;
        }
        self.window.request_redraw();
    }

    fn request_create_new(&self, _parent: WebView, request: CreateNewWebViewRequest) {
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        let webview = request
            .builder(self.content_context.clone())
            .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
            .delegate(me)
            .build();
        self.adopt_tab(webview);
    }

    fn notify_closed(&self, webview: WebView) {
        if let Some(i) = self.tab_index(&webview) {
            self.close_tab(i);
        }
    }

    fn show_embedder_control(&self, _webview: WebView, control: EmbedderControl) {
        match control {
            EmbedderControl::SimpleDialog(dialog) => {
                let (kind, input) = match &dialog {
                    SimpleDialog::Alert(_) => (SimpleKind::Alert, String::new()),
                    SimpleDialog::Confirm(_) => (SimpleKind::Confirm, String::new()),
                    SimpleDialog::Prompt(p) => (SimpleKind::Prompt, p.current_value().to_string()),
                };
                let message = dialog.message().to_string();
                self.push_dialog(Dialog::Simple {
                    kind,
                    message,
                    input,
                    handle: Some(dialog),
                });
            }
            EmbedderControl::SelectElement(select) => {
                let mut options = Vec::new();
                for item in select.options() {
                    match item {
                        SelectElementOptionOrOptgroup::Option(o) => options.push(SelectOpt {
                            id: Some(o.id),
                            label: o.label.clone(),
                            disabled: o.is_disabled,
                        }),
                        SelectElementOptionOrOptgroup::Optgroup { label, options: opts } => {
                            options.push(SelectOpt {
                                id: None,
                                label: label.clone(),
                                disabled: true,
                            });
                            for o in opts {
                                options.push(SelectOpt {
                                    id: Some(o.id),
                                    label: o.label.clone(),
                                    disabled: o.is_disabled,
                                });
                            }
                        }
                    }
                }
                self.push_dialog(Dialog::Select {
                    options,
                    handle: Some(select),
                });
            }
            EmbedderControl::ColorPicker(picker) => {
                let cur = picker.current_color().unwrap_or(RgbColor {
                    red: 0,
                    green: 0,
                    blue: 0,
                });
                self.push_dialog(Dialog::Color {
                    hex: format!("#{:02x}{:02x}{:02x}", cur.red, cur.green, cur.blue),
                    handle: Some(picker),
                });
            }
            EmbedderControl::FilePicker(picker) => {
                let mut dialog = FileDialog::new();
                if !picker.filter_patterns().is_empty() {
                    let patterns: Vec<FilterPattern> = picker.filter_patterns().to_owned();
                    let filter = Filter::new(move |path: &Path| {
                        path.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
                            let ext = ext.to_lowercase();
                            patterns.iter().any(|p| ext == p.0)
                        })
                    });
                    dialog = dialog
                        .add_file_filter("All Supported Types", filter)
                        .default_file_filter("All Supported Types");
                }
                self.push_dialog(Dialog::File {
                    dialog,
                    handle: Some(picker),
                });
            }
            // IME: not yet implemented.
            _ => {}
        }
    }

    fn hide_embedder_control(&self, _webview: WebView, _control_id: EmbedderControlId) {
        // The page withdrew a control (e.g. navigated away mid-dialog). Drop pending
        // engine-backed overlays (dropping the handle cancels); keep any context menu.
        self.dialogs
            .borrow_mut()
            .retain(|d| matches!(d, Dialog::ContextMenu { .. }));
        self.window.request_redraw();
    }

    fn request_authentication(&self, _webview: WebView, request: AuthenticationRequest) {
        let url = request.url().to_string();
        let message = if request.for_proxy() {
            format!("The proxy at {url} requires a username and password.")
        } else {
            format!("{url} requires a username and password.")
        };
        self.push_dialog(Dialog::Auth {
            message,
            user: String::new(),
            pass: String::new(),
            handle: Some(request),
        });
    }

    fn request_permission(&self, _webview: WebView, request: PermissionRequest) {
        let message = format!(
            "This site is requesting permission: {:?}",
            request.feature()
        );
        self.push_dialog(Dialog::Permission {
            message,
            handle: Some(request),
        });
    }

    /// A pipeline in this tab's webview panicked. Mark the tab crashed and navigate it to the
    /// internal `gator://crash` recovery page (served by `load_web_resource`), carrying the
    /// crashed URL + panic reason so the page can offer a Reload-back-to-that-URL button.
    /// `tab.load` re-spawns the pipeline, so the tab stays usable.
    fn notify_crashed(&self, webview: WebView, reason: String, _backtrace: Option<String>) {
        let Some(i) = self.tab_index(&webview) else {
            return;
        };
        let crashed_url = {
            let mut tabs = self.tabs.borrow_mut();
            tabs[i].crashed = true;
            tabs[i].loading = false;
            // Don't offer a reload back to our own crash page if it somehow crashes again.
            if tabs[i].url.starts_with("gator://crash") {
                String::new()
            } else {
                tabs[i].url.clone()
            }
        };
        let recovery = Url::parse_with_params(
            "gator://crash",
            &[("url", crashed_url.as_str()), ("reason", reason.as_str())],
        );
        if let Ok(recovery) = recovery {
            webview.load(recovery);
        }
        self.window.request_redraw();
    }

    fn notify_fullscreen_state_changed(&self, _webview: WebView, is_fullscreen: bool) {
        self.fullscreen.set(is_fullscreen);
        self.window.request_redraw();
    }

    fn request_navigation(&self, _webview: WebView, navigation_request: NavigationRequest) {
        navigation_request.allow();
    }
}

enum App {
    Initial {
        waker: Waker,
        ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    },
    Running(Rc<AppState>),
}

impl ApplicationHandler<WakeUp> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (waker, ipc_clients) = match self {
            App::Initial { waker, ipc_clients } => (waker.clone(), ipc_clients.clone()),
            App::Running(_) => return,
        };
        let event_proxy = waker.0.clone();

        let display_handle = event_loop.display_handle().expect("no display handle");
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title("NavGator")
                    .with_decorations(false)
                    .with_visible(false)
                    .with_inner_size(LogicalSize::new(1280.0, 800.0)),
            )
            .expect("failed to create window");
        let window_handle = window.window_handle().expect("no window handle");

        let inner = window.inner_size();
        let scale = window.scale_factor();

        let window_context = Rc::new(
            WindowRenderingContext::new(display_handle, window_handle, inner)
                .expect("failed to create WindowRenderingContext"),
        );
        let _ = window_context.make_current();
        let content_context = Rc::new(window_context.offscreen_context(inner));

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker))
            .preferences(navgator_preferences())
            .build();
        servo.setup_logging();

        let _ = content_context.make_current();
        let egui = EguiGlow::new(event_loop, content_context.glow_gl_api(), None, None, false);
        egui.egui_ctx.options_mut(|o| {
            o.zoom_with_keyboard = false;
        });
        window.set_visible(true);

        let (sync_cb, sync_ch, sync_cp) = load_sync_cursors();
        let state = Rc::new(AppState {
            servo,
            window_context,
            content_context,
            egui: RefCell::new(egui),
            toolbar_height: Cell::new(0.0),
            content_px: Cell::new((0, 0)),
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            location: RefCell::new(String::new()),
            location_dirty: Cell::new(false),
            focus_omnibox: Cell::new(false),
            show_settings: Cell::new(false),
            sync_cursor_bookmarks: Cell::new(sync_cb),
            sync_cursor_history: Cell::new(sync_ch),
            sync_cursor_passwords: Cell::new(sync_cp),
            adblock: adblock::Engine::from_rules(
                load_filter_rules(),
                adblock::lists::ParseOptions::default(),
            ),
            adblock_blocked: Cell::new(0),
            pending_cosmetic: RefCell::new(Vec::new()),
            sync_status: RefCell::new(String::new()),
            syncing: Cell::new(false),
            downloads: RefCell::new(Vec::new()),
            download_toast: RefCell::new(None),
            password_store: RefCell::new(password::PasswordStore::load(
                config_file("passwords.enc").unwrap_or_else(|| PathBuf::from("passwords.enc")),
            )),
            password_input: RefCell::new(String::new()),
            password_msg: RefCell::new(None),
            dialogs: RefCell::new(Vec::new()),
            closed_tabs: RefCell::new(Vec::new()),
            find_open: Cell::new(false),
            find_query: RefCell::new(String::new()),
            find_matches: Cell::new(0),
            find_active: Cell::new(0),
            find_focus: Cell::new(false),
            fullscreen: Cell::new(false),
            scale: Cell::new(scale),
            cursor: Cell::new((0.0, 0.0)),
            ctrl: Cell::new(false),
            shift: Cell::new(false),
            weak_self: RefCell::new(Weak::new()),
            ipc_clients,
            settings: RefCell::new(load_settings()),
            profile: RefCell::new(load_profile()),
            event_proxy,
            window,
        });
        *state.weak_self.borrow_mut() = Rc::downgrade(&state);

        // Restore the previous session's tabs unless the user passed an explicit URL on the
        // command line (which takes precedence). A missing/empty/malformed session yields no
        // tabs, in which case we open the welcome page exactly as before.
        let restored = if cli_url_given() {
            Vec::new()
        } else {
            load_session()
        };
        if restored.is_empty() {
            state.new_tab(content_url());
        } else {
            for url in restored {
                state.new_tab(url);
            }
        }

        *self = App::Running(state);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WakeUp) {
        if let App::Running(state) = self {
            match event {
                WakeUp::Exit => {
                    event_loop.exit();
                    return;
                }
                WakeUp::Ipc(cmd) => state.handle_ipc(cmd),
                WakeUp::SyncDone(outcome) => state.apply_sync(outcome),
                WakeUp::CosmeticReady => state.flush_cosmetic(),
                WakeUp::Wake => {}
            }
            state.servo.spin_event_loop();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let App::Running(state) = self else { return };
        state.servo.spin_event_loop();

        // Window-level events handled before egui.
        match &event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                state.update();
                state.paint();
                return;
            }
            WindowEvent::Resized(size) => {
                state.window_context.resize(*size);
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale.set(*scale_factor);
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor.set((position.x, position.y));
            }
            WindowEvent::ModifiersChanged(m) => {
                state.ctrl.set(m.state().control_key());
                state.shift.set(m.state().shift_key());
            }
            _ => {}
        }

        // Feed egui, then decide whether the event also goes to the page.
        let resp = state.egui.borrow_mut().on_window_event(&state.window, &event);
        if resp.repaint {
            state.window.request_redraw();
        }

        let scale = state.scale.get();
        let toolbar_dev = state.toolbar_dev();
        let (cx, cy) = state.cursor.get();
        let over_toolbar = cy < toolbar_dev;
        let dialog_open = !state.dialogs.borrow().is_empty();

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                match state.resize_direction_at(position.x, position.y) {
                    Some(dir) => state.window.set_cursor(resize_cursor(dir)),
                    None => state.window.set_cursor(CursorIcon::Default),
                }
                if !(resp.consumed || over_toolbar || dialog_open) {
                    state.forward_to_page(InputEvent::MouseMove(MouseMoveEvent::new(
                        DevicePoint::new(position.x as f32, (position.y - toolbar_dev) as f32).into(),
                    )));
                }
            }

            WindowEvent::MouseInput {
                state: bs,
                button,
                ..
            } => {
                // Borderless edge resize takes priority.
                if button == MouseButton::Left && bs == ElementState::Pressed {
                    if let Some(dir) = state.resize_direction_at(cx, cy) {
                        let _ = state.window.drag_resize_window(dir);
                        return;
                    }
                }
                // Right-click over the page → native context menu.
                if button == MouseButton::Right
                    && bs == ElementState::Pressed
                    && !over_toolbar
                    && !dialog_open
                {
                    state.push_dialog(Dialog::ContextMenu {
                        pos: egui::pos2((cx / scale) as f32, (cy / scale) as f32),
                    });
                    return;
                }
                // Drag the window from empty toolbar space.
                if button == MouseButton::Left
                    && bs == ElementState::Pressed
                    && over_toolbar
                    && !resp.consumed
                {
                    let _ = state.window.drag_window();
                    return;
                }
                if !(resp.consumed || over_toolbar || dialog_open) {
                    let action = match bs {
                        ElementState::Pressed => MouseButtonAction::Down,
                        ElementState::Released => MouseButtonAction::Up,
                    };
                    let servo_button = match button {
                        MouseButton::Left => ServoMouseButton::Left,
                        MouseButton::Right => ServoMouseButton::Right,
                        MouseButton::Middle => ServoMouseButton::Middle,
                        MouseButton::Back => ServoMouseButton::Back,
                        MouseButton::Forward => ServoMouseButton::Forward,
                        MouseButton::Other(v) => ServoMouseButton::Other(v),
                    };
                    if bs == ElementState::Pressed {
                        if let Some(tab) = state.active_tab() {
                            tab.focus();
                        }
                    }
                    state.forward_to_page(InputEvent::MouseButton(MouseButtonEvent::new(
                        action,
                        servo_button,
                        DevicePoint::new(cx as f32, (cy - toolbar_dev) as f32).into(),
                    )));
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if state.ctrl.get() {
                    let up = match delta {
                        MouseScrollDelta::LineDelta(_, ly) => ly > 0.0,
                        MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
                    };
                    if up {
                        state.zoom_in();
                    } else {
                        state.zoom_out();
                    }
                    return;
                }
                if !(resp.consumed || over_toolbar || dialog_open) {
                    let (dx, dy, mode) = match delta {
                        MouseScrollDelta::LineDelta(lx, ly) => {
                            ((lx * 76.0) as f64, (ly * 76.0) as f64, WheelMode::DeltaLine)
                        }
                        MouseScrollDelta::PixelDelta(p) => (p.x, p.y, WheelMode::DeltaPixel),
                    };
                    let wheel = WheelDelta {
                        x: dx,
                        y: dy,
                        z: 0.0,
                        mode,
                    };
                    state.forward_to_page(InputEvent::Wheel(WheelEvent::new(
                        wheel,
                        DevicePoint::new(cx as f32, (cy - toolbar_dev) as f32).into(),
                    )));
                }
            }

            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                // Ctrl-based shortcuts are handled here and not forwarded.
                if matches!(key_event.state, ElementState::Pressed) && state.ctrl.get() {
                    match &key_event.logical_key {
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("t") => {
                            if state.shift.get() {
                                state.reopen_closed_tab();
                            } else {
                                state.new_tab(content_url());
                            }
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("w") => {
                            state.close_tab(state.active.get());
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("l") => {
                            state.focus_omnibox.set(true);
                            state.window.request_redraw();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("r") => {
                            if let Some(tab) = state.active_tab() {
                                tab.reload();
                            }
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("d") => {
                            state.toggle_bookmark_active();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("f") => {
                            state.find_open.set(true);
                            state.find_focus.set(true);
                            state.window.request_redraw();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("h") => {
                            if let (Ok(url), Some(tab)) =
                                (Url::parse("gator://history"), state.active_tab())
                            {
                                state.location_dirty.set(false);
                                tab.load(url);
                            }
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("j") => {
                            if let (Ok(url), Some(tab)) =
                                (Url::parse("gator://downloads"), state.active_tab())
                            {
                                state.location_dirty.set(false);
                                tab.load(url);
                            }
                            return;
                        }
                        WinitKey::Character(c) if c == "=" || c == "+" => {
                            state.zoom_in();
                            return;
                        }
                        WinitKey::Character(c) if c == "-" || c == "_" => {
                            state.zoom_out();
                            return;
                        }
                        WinitKey::Character(c) if c == "0" => {
                            state.zoom_reset();
                            return;
                        }
                        WinitKey::Character(c) => {
                            if let Ok(n) = c.parse::<usize>() {
                                let len = state.tabs.borrow().len();
                                if n == 9 && len > 0 {
                                    state.select_tab(len - 1);
                                } else if (1..=8).contains(&n) && n <= len {
                                    state.select_tab(n - 1);
                                }
                                return;
                            }
                        }
                        WinitKey::Named(NamedKey::Tab) => {
                            let len = state.tabs.borrow().len();
                            if len > 1 {
                                let cur = state.active.get();
                                let next = if state.shift.get() {
                                    (cur + len - 1) % len
                                } else {
                                    (cur + 1) % len
                                };
                                state.select_tab(next);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                // F5 reloads the active tab (Ctrl+R is handled above; F5 carries no Ctrl).
                if matches!(key_event.state, ElementState::Pressed)
                    && matches!(key_event.logical_key, WinitKey::Named(NamedKey::F5))
                {
                    if let Some(tab) = state.active_tab() {
                        tab.reload();
                    }
                    return;
                }
                // Esc closes a context menu, else exits page fullscreen.
                if matches!(key_event.state, ElementState::Pressed)
                    && matches!(key_event.logical_key, WinitKey::Named(NamedKey::Escape))
                {
                    if state.find_open.get() {
                        state.find_close();
                        return;
                    }
                    if !state.dialogs.borrow().is_empty() {
                        state.dialogs.borrow_mut().clear();
                        state.window.request_redraw();
                        return;
                    }
                    if state.fullscreen.get() {
                        if let Some(tab) = state.active_tab() {
                            tab.exit_fullscreen();
                        }
                        return;
                    }
                }
                if !(resp.consumed || dialog_open) {
                    if let Some(key) = winit_key_to_servo(&key_event.logical_key) {
                        let key_state = match key_event.state {
                            ElementState::Pressed => KeyState::Down,
                            ElementState::Released => KeyState::Up,
                        };
                        state.forward_to_page(InputEvent::Keyboard(
                            KeyboardEvent::from_state_and_key(key_state, key),
                        ));
                    }
                }
            }

            _ => {}
        }
    }
}

/// Bridges Servo's "wake the UI thread" requests onto the winit event loop.
#[derive(Clone)]
struct Waker(EventLoopProxy<WakeUp>);

/// Events posted to the winit loop from other threads.
#[derive(Debug)]
enum WakeUp {
    /// Servo asked us to wake and pump its event loop.
    Wake,
    /// A command arrived on the IPC control socket.
    Ipc(IpcCommand),
    /// A window-control/close request; exit the event loop.
    Exit,
    /// A background Lyku sync finished; apply the result on the UI thread.
    SyncDone(sync::SyncOutcome),
    /// Cosmetic-filter CSS is ready to inject (deferred out of the JS-eval callback).
    CosmeticReady,
}

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let _ = self.0.send_event(WakeUp::Wake);
    }
}

/// Bind the IPC control socket and accept connections on a background thread.
fn start_ipc(path: String, proxy: EventLoopProxy<WakeUp>, clients: Arc<Mutex<Vec<UnixStream>>>) {
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("navgator: could not bind IPC socket {path}: {e}");
            return;
        }
    };
    eprintln!("navgator: IPC control socket listening on {path}");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Ok(writer) = stream.try_clone() {
                clients.lock().unwrap().push(writer);
            }
            let proxy = proxy.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if let Some(cmd) = IpcCommand::parse(&line) {
                        let _ = proxy.send_event(WakeUp::Ipc(cmd));
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod adblock_tests {
    #[test]
    fn blocks_trackers_allows_content() {
        let engine = adblock::Engine::from_rules(
            include_str!("content/blocklist.txt").lines(),
            adblock::lists::ParseOptions::default(),
        );
        // a known tracker, loaded third-party from a content site, is blocked
        let ad = adblock::request::Request::new(
            "https://www.google-analytics.com/analytics.js",
            "https://news.example.com/article",
            "script",
        )
        .unwrap();
        assert!(
            engine.check_network_request(&ad).matched,
            "tracker should be blocked"
        );
        // the page's own first-party content is not blocked
        let page = adblock::request::Request::new(
            "https://news.example.com/article.html",
            "https://news.example.com/article",
            "document",
        )
        .unwrap();
        assert!(
            !engine.check_network_request(&page).matched,
            "first-party content must not be blocked"
        );
    }
}
