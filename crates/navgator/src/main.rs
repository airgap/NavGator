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

/// Resolve navgator's bundled web assets (the home page). A packaged build keeps them
/// next to the executable in `<exe_dir>/resources/content`; `cargo run` falls back to the
/// source tree.
fn resources_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let res = dir.join("resources");
            if res.join("content/home.html").exists() {
                return res;
            }
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn file_url(rel: &str) -> Url {
    let p = resources_dir().join(rel);
    Url::from_file_path(&p).unwrap_or_else(|_| Url::parse("about:blank").unwrap())
}

/// Built-in search engines offered in Settings; the first is the default. The welcome page
/// and the omnibox both substitute the query for `%s` in the selected template.
const SEARCH_ENGINES: &[(&str, &str)] = &[
    ("DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("Kagi", "https://kagi.com/search?q=%s"),
    ("Bing", "https://www.bing.com/search?q=%s"),
    ("Google", "https://www.google.com/search?q=%s"),
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

/// User settings, persisted to a small `key=value` config file.
#[derive(Clone)]
struct Settings {
    /// Search URL template; `%s` is replaced with the URL-encoded query.
    search: String,
    /// UI accent color (any CSS-style `#rrggbb`).
    accent: String,
    /// Dark chrome theme (vs light).
    dark: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search: "https://duckduckgo.com/?q=%s".to_string(),
            accent: "#5b8cff".to_string(),
            dark: true,
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
            format!("search={}\naccent={}\ndark={}\n", s.search, s.accent, s.dark),
        );
    }
}

/// One visited page (frecency = visit count, for autocomplete ranking later).
struct HistoryEntry {
    url: String,
    title: String,
    visits: u32,
}

struct Bookmark {
    url: String,
    title: String,
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

/// TSV cell sanitizer — fields can't contain the tab/newline separators.
fn tsv_field(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

fn load_profile() -> Profile {
    let mut p = Profile::default();
    if let Some(text) = config_file("history.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let mut it = line.splitn(3, '\t');
            if let (Some(u), Some(t), Some(v)) = (it.next(), it.next(), it.next()) {
                p.history.push(HistoryEntry {
                    url: u.to_string(),
                    title: t.to_string(),
                    visits: v.parse().unwrap_or(1),
                });
            }
        }
    }
    if let Some(text) = config_file("bookmarks.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let mut it = line.splitn(2, '\t');
            if let (Some(u), Some(t)) = (it.next(), it.next()) {
                p.bookmarks.push(Bookmark {
                    url: u.to_string(),
                    title: t.to_string(),
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
            .map(|e| format!("{}\t{}\t{}\n", tsv_field(&e.url), tsv_field(&e.title), e.visits))
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
            .map(|b| format!("{}\t{}\n", tsv_field(&b.url), tsv_field(&b.title)))
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
        include_str!("content/welcome.html")
            .replace("__ACCENT__", &accent)
            .replace("__SEARCH_TEMPLATE__", &search)
            .replace("__SEARCH_ENGINE__", engine)
            .replace("__BOOKMARKS__", &bookmarks)
            .into_bytes()
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
                        let count = self.tabs.borrow().len();
                        for i in 0..count {
                            let (title, fav, loading) = {
                                let tabs = self.tabs.borrow();
                                let fav = tabs[i].favicon_tex.as_ref().map(|t| {
                                    egui::load::SizedTexture::new(t.id(), egui::vec2(16.0, 16.0))
                                });
                                (tabs[i].title.clone(), fav, tabs[i].loading)
                            };
                            if loading {
                                ui.add(egui::Spinner::new().size(14.0));
                            } else if let Some(sized) = fav {
                                ui.add(
                                    egui::Image::from_texture(sized)
                                        .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                                );
                            }
                            let tab = ui.add(egui::Button::selectable(
                                i == active,
                                truncate_ellipsis(&title, 20),
                            ));
                            if tab.clicked() && i != active {
                                self.select_tab(i);
                            }
                            if tab.middle_clicked() {
                                self.close_tab(i);
                                break;
                            }
                            let mut menu_act = 0u8;
                            tab.context_menu(|ui| {
                                if ui.button("New tab").clicked() {
                                    menu_act = 1;
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
                                _ => {}
                            }
                            if ui.add(egui::Button::new("×").frame(false)).clicked() {
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
                ui.label("Accent color (#rrggbb)");
                changed |= ui.text_edit_singleline(&mut s.accent).changed();
                ui.add_space(6.0);
                changed |= ui.checkbox(&mut s.dark, "Dark theme").changed();
                if changed {
                    save_settings(&s);
                }
            });
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
            });
            tabs.len() - 1
        };
        self.select_tab(idx);
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
                } else {
                    tab.webview.hide();
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
    }

    /// Close every tab except `keep` (tab context menu).
    fn close_others(&self, keep: usize) {
        {
            let tabs = self.tabs.borrow();
            if keep >= tabs.len() {
                return;
            }
            let mut closed = self.closed_tabs.borrow_mut();
            for (j, t) in tabs.iter().enumerate() {
                if j != keep && !t.url.is_empty() {
                    closed.push(t.url.clone());
                }
            }
        }
        {
            let mut tabs = self.tabs.borrow_mut();
            let kept = tabs.remove(keep);
            tabs.clear();
            tabs.push(kept);
        }
        self.active.set(0);
        self.select_tab(0);
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
    fn record_visit(&self, url: &str, title: &str) {
        if url.is_empty()
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
        } else {
            p.history.push(HistoryEntry {
                url: url.to_string(),
                title: title.to_string(),
                visits: 1,
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
            p.bookmarks.push(Bookmark { url, title });
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
    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        if load.request().url.scheme() != "gator" {
            return;
        }
        let url = load.request().url.clone();
        let body = match url.host_str().unwrap_or("welcome") {
            "welcome" | "newtab" | "home" => self.render_gator_welcome(),
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
                // A new load clears any stale hover/status text.
                if !matches!(status, LoadStatus::Complete) {
                    tabs[i].status_text = None;
                }
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

        state.new_tab(content_url());

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
