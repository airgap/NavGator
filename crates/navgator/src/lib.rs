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
    AuthenticationRequest, ColorPicker, ConsoleLogLevel, CreateNewWebViewRequest, Cursor, DeviceIntRect,
    DeviceIntSize, DevicePoint, EmbedderControl,
    EmbedderControlId, EventLoopWaker, FilePicker, FilterPattern, Image, InputEvent, InputEventId,
    InputEventResult, JSValue, Key,
    KeyState, KeyboardEvent, LoadStatus, MediaSessionEvent, MediaSessionPlaybackState, Modifiers,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey as ServoNamedKey, NavigationRequest, OffscreenRenderingContext, Opts, PermissionRequest,
    PixelFormat, Preferences, RenderingContext, run_content_process,
    SandboxOutcome, apply_sandbox, content_process_policy,
    RgbColor, SelectElement, SelectElementOptionOrOptgroup, Servo, ServoBuilder, SimpleDialog,
    Theme as ServoTheme,
    UserContentManager, UserScript, WebResourceLoad, WebResourceResponse, WebView, WebViewBuilder,
    WebViewDelegate, WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
// `http` types for building the WebResourceResponse served to gator:// internal pages.
use navgator_engine::http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderName, USER_AGENT},
};
use navgator_protocol::IpcCommand;

mod sync;
mod password;
mod keyring_store;
mod theme;
mod fonts;
mod palette;
mod userscripts;
mod archive;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowId};
use std::collections::HashMap;

/// Width of the invisible window-edge band (logical px) that starts a resize on the
/// borderless window — OS decorations are off, so we hit-test it and `drag_resize_window`.
const RESIZE_BORDER: f64 = 6.0;

/// Page-zoom step + bounds (Ctrl +/-/0, Ctrl+wheel).
const ZOOM_STEP: f32 = 1.1;
const ZOOM_MIN: f32 = 0.3;
const ZOOM_MAX: f32 = 3.0;

/// Android entry point — invoked by android-activity's NativeActivity glue. Built only for
/// Android (as a cdylib); the desktop binary calls `desktop_main` via the thin `src/main.rs`.
/// Mirrors `desktop_main`'s core run (crypto provider + event loop + app), minus the desktop-only
/// content-process re-exec, sandbox self-test, blocklist refresh and Unix-socket IPC.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");
    let event_loop = EventLoop::with_user_event()
        .with_android_app(app)
        .build()
        .expect("failed to build event loop");
    let ipc_clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    let mut app = App::Initial {
        waker: Waker(event_loop.create_proxy()),
        ipc_clients,
    };
    let _ = event_loop.run_app(&mut app);
}

/// Desktop entry point — the app's real `main`, invoked by the thin `src/main.rs` binary.
pub fn desktop_main() -> Result<(), Box<dyn Error>> {
    // Multiprocess content: when the engine's constellation needs a content process it re-execs
    // THIS binary with `--content-process <ipc-token>`. Hand straight off to the engine — before
    // any winit/egui/rustls init — or the child would try to open its own window.
    {
        let mut a = std::env::args();
        a.next(); // argv[0]
        match a.next().as_deref() {
            Some("--content-process") => {
                let token = a.next().expect("--content-process requires an IPC token");
                run_content_process(token);
                return Ok(());
            }
            // M1/M2 confinement gate (plan §8.1/§13.5): self-applies the SAME production policy in
            // this process, runs the negative-capability battery, exits 0 iff every hard gate denies.
            Some("--sandbox-selftest") => run_sandbox_selftest(),
            _ => {}
        }
    }
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
/// Returns `(pane0 urls, pane1 urls)`. A `#PANE1` marker line splits the two; without it (a
/// non-split session) everything lands in pane 0 and pane 1 is empty.
fn load_session() -> (Vec<Url>, Vec<Url>) {
    config_file("session.tsv")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_session(&t))
        .unwrap_or_default()
}

/// Load the permission ledger: `origin \t feature \t 1|0` per line.
fn load_permission_grants() -> HashMap<(String, String), bool> {
    let mut m = HashMap::new();
    if let Some(text) = config_file("permissions.tsv").and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let mut it = line.splitn(3, '\t');
            if let (Some(o), Some(f), Some(v)) = (it.next(), it.next(), it.next()) {
                m.insert((o.to_string(), f.to_string()), v.trim() == "1");
            }
        }
    }
    m
}

/// Persist the permission ledger.
fn save_permission_grants(m: &HashMap<(String, String), bool>) {
    let Some(path) = config_file("permissions.tsv") else {
        return;
    };
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let s: String = m
        .iter()
        .map(|((o, f), v)| format!("{}\t{}\t{}\n", o, f, if *v { "1" } else { "0" }))
        .collect();
    let _ = std::fs::write(path, s);
}

/// Pure parser for the session file (see `load_session` / `save_session`). Lines are URLs; a
/// `#PANE1` marker switches subsequent URLs to pane 1; non-URL lines are skipped.
fn parse_session(text: &str) -> (Vec<Url>, Vec<Url>) {
    let mut p0 = Vec::new();
    let mut p1 = Vec::new();
    let mut in_pane1 = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "#PANE1" {
            in_pane1 = true;
            continue;
        }
        if let Ok(u) = Url::parse(line) {
            if in_pane1 {
                p1.push(u);
            } else {
                p0.push(u);
            }
        }
    }
    (p0, p1)
}

/// Scan installed Chromium-family browsers for their JSON `Bookmarks` file and return every
/// bookmarked (url, title). Read-only, no SQLite, no lock — Chrome stores bookmarks as plain JSON.
fn import_chrome_bookmarks() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return out;
    };
    let candidates = [
        ".config/google-chrome/Default/Bookmarks",
        ".config/chromium/Default/Bookmarks",
        ".config/BraveSoftware/Brave-Browser/Default/Bookmarks",
        ".config/microsoft-edge/Default/Bookmarks",
        ".config/vivaldi/Default/Bookmarks",
    ];
    for rel in candidates {
        let Ok(text) = std::fs::read_to_string(home.join(rel)) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(roots) = json.get("roots").and_then(|r| r.as_object()) {
            for node in roots.values() {
                collect_chrome_bookmarks(node, &mut out);
            }
        }
    }
    out
}

/// Recursively walk a Chrome bookmark node, pushing every `type:"url"` entry.
fn collect_chrome_bookmarks(node: &serde_json::Value, out: &mut Vec<(String, String)>) {
    if node.get("type").and_then(|t| t.as_str()) == Some("url") {
        if let Some(url) = node.get("url").and_then(|u| u.as_str()) {
            if url.starts_with("http") {
                let name = node.get("name").and_then(|n| n.as_str()).unwrap_or(url);
                out.push((url.to_string(), name.to_string()));
            }
        }
        return;
    }
    if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_chrome_bookmarks(child, out);
        }
    }
}

/// Open a Chrome/Firefox SQLite DB read-only, even while the browser holds it: `immutable=1`
/// tells SQLite the file won't change, so the live lock + WAL aren't an obstacle for a one-shot
/// import. Returns None if the file is absent or won't open.
fn open_browser_db(path: &std::path::Path) -> Option<rusqlite::Connection> {
    if !path.exists() {
        return None;
    }
    let uri = format!("file:{}?immutable=1", path.display());
    rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

/// The Firefox profiles' `places.sqlite` paths (each profile dir may have one).
fn firefox_places() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        if let Ok(entries) = std::fs::read_dir(home.join(".mozilla/firefox")) {
            for entry in entries.flatten() {
                let p = entry.path().join("places.sqlite");
                if p.exists() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Bookmarks from every Firefox profile (moz_bookmarks joined to moz_places).
fn import_firefox_bookmarks() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for path in firefox_places() {
        let Some(conn) = open_browser_db(&path) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT p.url, b.title FROM moz_bookmarks b \
             JOIN moz_places p ON b.fk = p.id \
             WHERE b.type = 1 AND p.url LIKE 'http%'",
        ) else {
            continue;
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            let url: String = r.get(0)?;
            let title: String = r.get::<_, Option<String>>(1)?.unwrap_or_else(|| url.clone());
            Ok((url, title))
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Recent history (url, title, visits, updated_ms) from Chrome-family + Firefox, newest first.
fn import_browser_history() -> Vec<(String, String, u32, i64)> {
    const LIMIT: usize = 2000;
    let mut out = Vec::new();
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return out;
    };
    // Chrome family: `urls.last_visit_time` is microseconds since 1601.
    let chrome_dirs = [
        "google-chrome",
        "chromium",
        "BraveSoftware/Brave-Browser",
        "microsoft-edge",
        "vivaldi",
    ];
    for rel in chrome_dirs {
        let path = home.join(".config").join(rel).join("Default").join("History");
        let Some(conn) = open_browser_db(&path) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT url, title, visit_count, last_visit_time FROM urls \
             WHERE url LIKE 'http%' ORDER BY last_visit_time DESC LIMIT {LIMIT}"
        )) else {
            continue;
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            let url: String = r.get(0)?;
            let title: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let visits: i64 = r.get(2).unwrap_or(1);
            let t: i64 = r.get(3).unwrap_or(0);
            let updated = if t > 0 { (t - 11_644_473_600_000_000) / 1000 } else { 0 };
            Ok((url, title, visits.max(0) as u32, updated))
        }) {
            out.extend(rows.flatten());
        }
    }
    // Firefox: `moz_places.last_visit_date` is microseconds since 1970.
    for path in firefox_places() {
        let Some(conn) = open_browser_db(&path) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT url, title, visit_count, last_visit_date FROM moz_places \
             WHERE url LIKE 'http%' AND last_visit_date IS NOT NULL \
             ORDER BY last_visit_date DESC LIMIT {LIMIT}"
        )) else {
            continue;
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            let url: String = r.get(0)?;
            let title: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let visits: i64 = r.get(2).unwrap_or(1);
            let t: i64 = r.get(3).unwrap_or(0);
            Ok((url, title, visits.max(0) as u32, t / 1000))
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Register NavGator as the system default browser: write a `.desktop` launcher pointing at the
/// running binary (with `%u`, so links are passed through) and set it via xdg-settings, falling
/// back to xdg-mime. Returns a human-readable status.
fn set_default_browser() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "Could not locate the NavGator binary.".into();
    };
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return "No HOME directory.".into();
    };
    let apps = home.join(".local/share/applications");
    if std::fs::create_dir_all(&apps).is_err() {
        return "Could not create ~/.local/share/applications.".into();
    }
    let desktop = apps.join("navgator.desktop");
    let content = format!(
        "[Desktop Entry]\nVersion=1.0\nName=NavGator\nComment=A fast, private web browser\n\
         Exec={} %u\nTerminal=false\nType=Application\nCategories=Network;WebBrowser;\n\
         MimeType=x-scheme-handler/http;x-scheme-handler/https;text/html;\nStartupNotify=true\n",
        exe.display()
    );
    if std::fs::write(&desktop, &content).is_err() {
        return "Could not write navgator.desktop.".into();
    }
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .output();
    let ok = std::process::Command::new("xdg-settings")
        .args(["set", "default-web-browser", "navgator.desktop"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::process::Command::new("xdg-mime")
            .args([
                "default",
                "navgator.desktop",
                "x-scheme-handler/http",
                "x-scheme-handler/https",
                "text/html",
            ])
            .output();
    }
    "NavGator is now your default browser — http/https links will open here.".into()
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
    /// Remember the sync passphrase in the OS keyring (Secret Service) for auto-unlock.
    remember_passphrase: bool,
    /// Block ads + trackers (adblock-rust). On by default — it's the pitch.
    block_ads: bool,
    /// Force a dark appearance on http(s) pages (CSS invert + hue-rotate, media re-inverted).
    force_dark: bool,
    /// New-tab page wallpaper (image URL); empty = the plain themed background.
    wallpaper: String,
    /// The live-customization chrome theme (OKLCH base/accent, density, fonts,
    /// radius, glass, tab placement). The single source of truth for the chrome
    /// look; `accent`/`dark` above are kept in sync from it for the gator:// pages.
    theme: theme::Theme,
    /// New-tab dashboard widget toggles (Studio "New-tab modules" section).
    modules: theme::Modules,
    /// True once the user has explicitly chosen a theme base (Studio surface chip). Until then
    /// the base follows the OS colour-scheme (dark OS → dark chrome) on each launch.
    theme_base_explicit: bool,
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
            remember_passphrase: false,
            block_ads: true,
            force_dark: false,
            wallpaper: String::new(),
            theme: theme::Theme::default(),
            modules: theme::Modules::default(),
            theme_base_explicit: false,
        }
    }
}

/// Whether the OS is in dark mode. `NAVGATOR_COLOR_SCHEME=dark|light` forces it (useful when the
/// desktop exposes no theme, and for testing); otherwise winit's per-window OS theme is used.
fn os_prefers_dark(window: &Window) -> bool {
    match std::env::var("NAVGATOR_COLOR_SCHEME").ok().as_deref() {
        Some("dark") => return true,
        Some("light") => return false,
        _ => {},
    }
    matches!(window.theme(), Some(winit::window::Theme::Dark))
}

/// Format an egui color as a `#rrggbb` string (for the gator:// page palette).
fn color_hex(c: egui::Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

/// True only for a strict CSS hex color (`#rgb` or `#rrggbb`). The legacy `accent` string is
/// interpolated raw into a `<style>` block on the privileged gator:// pages, so anything looser
/// (e.g. `#x}</style><script>…`) would be a stored-XSS vector — reject it at every write site.
fn is_hex_color(s: &str) -> bool {
    matches!(s.strip_prefix('#'),
        Some(x) if (x.len() == 3 || x.len() == 6) && x.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Levenshtein edit distance between two strings (for the Credential Firewall's typosquat check).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Flat vector chrome icons drawn via the egui painter — native, crisp, theme-coloured, and immune
/// to missing font glyphs (emoji/symbols like the puzzle, close-✕ and maximize-▢ otherwise render
/// as `.notdef` squares without an emoji font). Each fn paints into the icon's square bounding box.
/// A cached GL program (fullscreen SDF pass) that erases the four window corners to transparent,
/// giving the undecorated window rounded corners. Drawn after egui paints, before present, onto
/// the same (transparent) surface — a real compositor shows the corners through; the
/// compositor-less test display shows them black.
struct CornerMaskGl {
    program: glow::Program,
    vao: glow::VertexArray,
    u_res: glow::UniformLocation,
    u_radius: glow::UniformLocation,
}

impl CornerMaskGl {
    /// Compile the program + a fullscreen-triangle VAO. Returns `None` if any GL step fails (the
    /// caller then just leaves the corners square — no crash, no per-frame retry).
    fn build(gl: &glow::Context) -> Option<CornerMaskGl> {
        use glow::HasContext as _;
        unsafe {
            let sl = gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION);
            let header = if sl.contains("ES") {
                "#version 300 es\nprecision highp float;\n"
            } else {
                "#version 330 core\n"
            };
            let vs_src =
                format!("{header}layout(location=0) in vec2 a_pos;\nvoid main(){{ gl_Position = vec4(a_pos, 0.0, 1.0); }}\n");
            let fs_src = format!(
                "{header}out vec4 fragColor;\nuniform vec2 u_res;\nuniform float u_radius;\nvoid main() {{\n  vec2 p = gl_FragCoord.xy;\n  vec2 hs = u_res * 0.5;\n  vec2 q = abs(p - hs) - (hs - vec2(u_radius));\n  float d = length(max(q, vec2(0.0))) - u_radius;\n  float outside = clamp(d + 0.5, 0.0, 1.0);\n  fragColor = vec4(0.0, 0.0, 0.0, outside);\n}}\n"
            );

            let program = gl.create_program().ok()?;
            let mut ok = true;
            let mut shaders = Vec::new();
            for (ty, src) in [
                (glow::VERTEX_SHADER, &vs_src),
                (glow::FRAGMENT_SHADER, &fs_src),
            ] {
                match gl.create_shader(ty) {
                    Ok(sh) => {
                        gl.shader_source(sh, src);
                        gl.compile_shader(sh);
                        if !gl.get_shader_compile_status(sh) {
                            ok = false;
                        }
                        gl.attach_shader(program, sh);
                        shaders.push(sh);
                    },
                    Err(_) => ok = false,
                }
            }
            if ok {
                gl.link_program(program);
                ok = gl.get_program_link_status(program);
            }
            for sh in shaders {
                gl.detach_shader(program, sh);
                gl.delete_shader(sh);
            }
            if !ok {
                gl.delete_program(program);
                return None;
            }

            let vao = gl.create_vertex_array().ok()?;
            let vbo = gl.create_buffer().ok()?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let verts: [f32; 6] = [-1.0, -1.0, 3.0, -1.0, -1.0, 3.0];
            let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, std::mem::size_of_val(&verts));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            gl.bind_vertex_array(None);

            let u_res = gl.get_uniform_location(program, "u_res")?;
            let u_radius = gl.get_uniform_location(program, "u_radius")?;
            Some(CornerMaskGl { program, vao, u_res, u_radius })
        }
    }
}

mod icon {
    use egui::{pos2, vec2, Color32, CornerRadius, Painter, Rect, Stroke, StrokeKind};
    fn s(c: Color32) -> Stroke {
        Stroke::new(1.6, c)
    }
    pub fn close(p: &Painter, r: Rect, c: Color32) {
        p.line_segment([r.left_top(), r.right_bottom()], s(c));
        p.line_segment([r.right_top(), r.left_bottom()], s(c));
    }
    pub fn minimize(p: &Painter, r: Rect, c: Color32) {
        let y = r.center().y;
        p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], s(c));
    }
    /// Single rounded square, or two offset squares ("restore") when the window is maximized.
    pub fn maximize(p: &Painter, r: Rect, c: Color32, maximized: bool) {
        if maximized {
            let d = 3.0;
            let back = Rect::from_min_size(pos2(r.left() + d, r.top()), vec2(r.width() - d, r.height() - d));
            let front = Rect::from_min_size(pos2(r.left(), r.top() + d), vec2(r.width() - d, r.height() - d));
            p.rect_stroke(back, CornerRadius::same(1), s(c), StrokeKind::Inside);
            p.rect_stroke(front, CornerRadius::same(1), s(c), StrokeKind::Inside);
        } else {
            p.rect_stroke(r, CornerRadius::same(2), s(c), StrokeKind::Inside);
        }
    }
    pub fn menu(p: &Painter, r: Rect, c: Color32) {
        for t in [0.18_f32, 0.5, 0.82] {
            let y = r.top() + r.height() * t;
            p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], s(c));
        }
    }
    /// 2×2 swatch grid — "customize / themes" (the Studio).
    pub fn studio(p: &Painter, r: Rect, c: Color32) {
        let g = 1.6;
        let cell = (r.width() - g) / 2.0;
        for (i, j) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let tl = pos2(r.left() + i as f32 * (cell + g), r.top() + j as f32 * (cell + g));
            p.rect_filled(Rect::from_min_size(tl, vec2(cell, cell)), CornerRadius::same(1), c);
        }
    }
    /// Puzzle piece (extensions / add-ons): a rounded square with knobs on the top + right edges.
    pub fn addons(p: &Painter, r: Rect, c: Color32) {
        // `</>` code brackets — userscripts / add-ons.
        let st = Stroke::new(1.7, c);
        let (yt, yb) = (r.top() + r.height() * 0.16, r.bottom() - r.height() * 0.16);
        p.line_segment([pos2(r.left() + r.width() * 0.26, yt), pos2(r.left(), r.center().y)], st);
        p.line_segment([pos2(r.left(), r.center().y), pos2(r.left() + r.width() * 0.26, yb)], st);
        p.line_segment([pos2(r.right() - r.width() * 0.26, yt), pos2(r.right(), r.center().y)], st);
        p.line_segment([pos2(r.right(), r.center().y), pos2(r.right() - r.width() * 0.26, yb)], st);
        p.line_segment([pos2(r.center().x + r.width() * 0.13, yt), pos2(r.center().x - r.width() * 0.13, yb)], st);
    }
    /// Left / right chevrons (back / forward).
    pub fn back(p: &Painter, r: Rect, c: Color32) {
        chevron(p, r, c, true);
    }
    pub fn forward(p: &Painter, r: Rect, c: Color32) {
        chevron(p, r, c, false);
    }
    fn chevron(p: &Painter, r: Rect, c: Color32, left: bool) {
        let st = Stroke::new(1.8, c);
        let (tip_x, end_x) = if left {
            (r.left() + r.width() * 0.34, r.left() + r.width() * 0.62)
        } else {
            (r.left() + r.width() * 0.66, r.left() + r.width() * 0.38)
        };
        let dy = r.height() * 0.26;
        p.line_segment([pos2(end_x, r.center().y - dy), pos2(tip_x, r.center().y)], st);
        p.line_segment([pos2(tip_x, r.center().y), pos2(end_x, r.center().y + dy)], st);
    }
    /// Up / down chevrons (find prev / next match).
    pub fn up(p: &Painter, r: Rect, c: Color32) {
        vchevron(p, r, c, true);
    }
    pub fn down(p: &Painter, r: Rect, c: Color32) {
        vchevron(p, r, c, false);
    }
    fn vchevron(p: &Painter, r: Rect, c: Color32, up: bool) {
        let st = Stroke::new(1.8, c);
        let (tip_y, end_y) = if up {
            (r.top() + r.height() * 0.34, r.top() + r.height() * 0.62)
        } else {
            (r.top() + r.height() * 0.66, r.top() + r.height() * 0.38)
        };
        let dx = r.width() * 0.26;
        p.line_segment([pos2(r.center().x - dx, end_y), pos2(r.center().x, tip_y)], st);
        p.line_segment([pos2(r.center().x, tip_y), pos2(r.center().x + dx, end_y)], st);
    }
    /// Circular reload arrow: a near-full ring with a clean gap at the top and a filled arrowhead
    /// at the right side of the gap, pointing up into it — the conventional "refresh" look.
    pub fn reload(p: &Painter, r: Rect, c: Color32) {
        use std::f32::consts::{FRAC_PI_2, TAU};
        let st = Stroke::new(1.7, c);
        let center = r.center();
        let rad = r.width() * 0.32;
        let g = 0.62_f32; // half-width of the top gap, radians
        let start = -FRAC_PI_2 + g; // just clockwise (right) of 12 o'clock
        let sweep = TAU - 2.0 * g; // most of the circle
        let steps = 48;
        let pts: Vec<_> = (0..=steps)
            .map(|i| {
                let a = start + sweep * (i as f32 / steps as f32);
                pos2(center.x + rad * a.cos(), center.y + rad * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, st));
        // Arrowhead at the START end (right of the gap), apex pointing up-left into the gap
        // (counter to the sweep), so the arrow reads as sweeping clockwise back to the top.
        let tip = pos2(center.x + rad * start.cos(), center.y + rad * start.sin());
        let tangent = vec2(start.sin(), -start.cos()); // toward the gap (up-left)
        let radial = vec2(start.cos(), start.sin()); // outward
        let apex = tip + tangent * 5.2;
        let b1 = tip + radial * 3.6;
        let b2 = tip - radial * 3.6;
        p.add(egui::Shape::convex_polygon(vec![apex, b1, b2], c, Stroke::NONE));
    }
    /// Key: a ring on the left, a stem to the right, two teeth.
    pub fn key(p: &Painter, r: Rect, c: Color32) {
        let rad = r.height() * 0.32;
        let ring = pos2(r.left() + rad, r.center().y);
        let y = r.center().y;
        p.circle_stroke(ring, rad, s(c));
        p.line_segment([pos2(ring.x + rad, y), pos2(r.right(), y)], s(c));
        p.line_segment([pos2(r.right() - 1.5, y), pos2(r.right() - 1.5, y + 3.5)], s(c));
        p.line_segment([pos2(r.right() - 5.0, y), pos2(r.right() - 5.0, y + 3.5)], s(c));
    }
}

/// A frameless flat-icon button: a fixed-size clickable cell with the icon painted by `draw`
/// (muted normally, full text colour on hover). Drop-in replacement for the old emoji/symbol
/// `egui::Button::new("…")` chrome buttons.
fn icon_button(
    ui: &mut egui::Ui,
    enabled: bool,
    tip: &str,
    pal: &theme::Palette,
    draw: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
) -> egui::Response {
    // Disabled icons sense hover only (never .clicked()) and render faded.
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0, 24.0), sense);
    let col = if !enabled {
        pal.muted.gamma_multiply(0.4)
    } else if resp.hovered() {
        pal.text
    } else {
        pal.muted
    };
    // Hover background: a rounded fill behind the glyph, matching the New Tab button. The corner
    // follows the theme (egui derives `widgets.hovered.corner_radius` from `Theme::radius_sm`).
    if enabled && resp.hovered() {
        let cr = ui.visuals().widgets.hovered.corner_radius;
        ui.painter().rect_filled(rect.shrink(2.0), cr, pal.elev);
    }
    let icon_box = egui::Rect::from_center_size(rect.center(), egui::vec2(13.0, 13.0));
    draw(ui.painter(), icon_box, col);
    // Chrome buttons keep the Default arrow cursor (no pointing-hand).
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

/// Resolve an omnibar entry to a load target: a literal URL, a bare domain promoted to https, or
/// a search via `search_template` (`%s` placeholder). A `javascript:` entry is ALWAYS routed to
/// search — never loaded — so address-bar `javascript:`/`javascript://…` self-XSS can't fire.
/// URL targets are also run through `strip_tracking_params` (search queries are left intact).
fn omnibox_target(raw: &str, search_template: &str) -> String {
    let is_js = raw.trim_start().to_ascii_lowercase().starts_with("javascript:");
    if raw.contains("://") && !is_js {
        strip_tracking_params(raw)
    } else if raw.contains('.') && !raw.contains(' ') && !is_js {
        strip_tracking_params(&format!("https://{raw}"))
    } else {
        search_template.replace("%s", &url_encode(raw))
    }
}

/// True for a known click/campaign tracking query parameter (case-insensitive).
fn is_tracking_param(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.starts_with("utm_")
        || matches!(
            k.as_str(),
            "fbclid" | "gclid" | "gclsrc" | "dclid" | "wbraid" | "gbraid" | "msclkid" | "yclid"
                | "twclid" | "ttclid" | "igshid" | "mc_eid" | "mc_cid" | "_hsenc" | "_hsmi"
                | "vero_id" | "oly_anon_id" | "oly_enc_id" | "rb_clickid" | "s_cid" | "mkt_tok"
                | "ml_subscriber" | "ml_subscriber_hash" | "spm" | "scm"
        )
}

/// Remove known tracking/decoration query params (utm_*, fbclid, gclid, …) from a URL, leaving the
/// rest intact. Returns the input unchanged if it doesn't parse or has no query.
fn strip_tracking_params(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_string();
    };
    if url.query().is_none() {
        return raw.to_string();
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut qp = url.query_pairs_mut();
        qp.clear();
        for (k, v) in &kept {
            qp.append_pair(k, v);
        }
    }
    if kept.is_empty() {
        url.set_query(None);
    }
    url.to_string()
}

/// Mirror the live `theme` into the legacy `accent`/`dark` fields the gator://
/// pages read, so internal pages follow the chrome theme without changes.
fn sync_legacy_theme(s: &mut Settings) {
    s.accent = color_hex(s.theme.palette().accent);
    s.dark = !s.theme.base.is_light();
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
                    // Reject a poisoned persisted accent on load too (defense in depth for XSS).
                    "accent" if is_hex_color(v.trim()) => s.accent = v.trim().to_string(),
                    "dark" => s.dark = v.trim() == "true",
                    "sync_api_key" => s.sync_api_key = v.trim().to_string(),
                    "sync_bookmarks" => s.sync_bookmarks = v.trim() == "true",
                    "sync_history" => s.sync_history = v.trim() == "true",
                    "sync_passwords" => s.sync_passwords = v.trim() == "true",
                    "remember_passphrase" => s.remember_passphrase = v.trim() == "true",
                    "block_ads" => s.block_ads = v.trim() == "true",
                    "force_dark" => s.force_dark = v.trim() == "true",
                    "wallpaper" => s.wallpaper = v.trim().to_string(),
                    "th_base" => {
                        if let Some(b) = theme::Base::from_key(v.trim()) {
                            s.theme.base = b;
                        }
                    }
                    "th_base_explicit" => s.theme_base_explicit = v.trim() == "true",
                    "th_accent" => {
                        if let Some(a) = theme::Accent::from_key(v.trim()) {
                            s.theme.accent = a;
                        }
                    }
                    "th_density" => {
                        if let Some(d) = theme::Density::from_key(v.trim()) {
                            s.theme.density = d;
                        }
                    }
                    "th_font" => {
                        if let Some(f) = theme::FontChoice::from_key(v.trim()) {
                            s.theme.font = f;
                        }
                    }
                    "th_tabpos" => {
                        if let Some(p) = theme::TabPos::from_key(v.trim()) {
                            s.theme.tab_pos = p;
                        }
                    }
                    "th_wallpaper" => {
                        if let Some(w) = theme::Wallpaper::from_key(v.trim()) {
                            s.theme.wallpaper = w;
                        }
                    }
                    "th_tabfit" => {
                        if let Some(f) = theme::TabFit::from_key(v.trim()) {
                            s.theme.tab_fit = f;
                        }
                    }
                    "th_radius" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            s.theme.radius = n.min(30);
                        }
                    }
                    "th_glass" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            s.theme.glass = n.min(60);
                        }
                    }
                    "th_tabmaxw" => {
                        if let Ok(n) = v.trim().parse::<u16>() {
                            s.theme.tab_max_w = n.clamp(120, 340);
                        }
                    }
                    "mod_clock" => s.modules.clock = v.trim() == "true",
                    "mod_search" => s.modules.search = v.trim() == "true",
                    "mod_sites" => s.modules.sites = v.trim() == "true",
                    "mod_notes" => s.modules.notes = v.trim() == "true",
                    "mod_feed" => s.modules.feed = v.trim() == "true",
                    _ => {}
                }
            }
        }
    }
    sync_legacy_theme(&mut s);
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
                "search={}\naccent={}\ndark={}\nsync_api_key={}\nsync_bookmarks={}\nsync_history={}\nsync_passwords={}\nremember_passphrase={}\nblock_ads={}\nforce_dark={}\nwallpaper={}\nth_base={}\nth_accent={}\nth_density={}\nth_font={}\nth_tabpos={}\nth_wallpaper={}\nth_tabfit={}\nth_radius={}\nth_glass={}\nth_tabmaxw={}\nth_base_explicit={}\nmod_clock={}\nmod_search={}\nmod_sites={}\nmod_notes={}\nmod_feed={}\n",
                s.search,
                s.accent,
                s.dark,
                s.sync_api_key,
                s.sync_bookmarks,
                s.sync_history,
                s.sync_passwords,
                s.remember_passphrase,
                s.block_ads,
                s.force_dark,
                s.wallpaper,
                s.theme.base.key(),
                s.theme.accent.key(),
                s.theme.density.key(),
                s.theme.font.key(),
                s.theme.tab_pos.key(),
                s.theme.wallpaper.key(),
                s.theme.tab_fit.key(),
                s.theme.radius,
                s.theme.glass,
                s.theme.tab_max_w,
                s.theme_base_explicit,
                s.modules.clock,
                s.modules.search,
                s.modules.sites,
                s.modules.notes,
                s.modules.feed,
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

/// Directory holding user-provided *.js userscripts (Greasemonkey-style). Each file is
/// injected into every page on load via Servo's UserContentManager.
fn userscripts_dir() -> Option<PathBuf> {
    config_file("userscripts")
}

/// Path of the add-on registry file (consent state: enabled/granted/content_hash), a sibling of
/// passwords.enc under the config dir. The `*.user.js`/`*.js` files remain the source of truth
/// for code; this JSON holds only state + consent.
fn addons_path() -> Option<PathBuf> {
    config_file("addons.json")
}

/// Read every `*.user.js` (preferred) and bare legacy `*.js` file in the userscripts dir
/// (non-recursive), returning `(path, source)` pairs in deterministic order. Best-effort:
/// unreadable files are skipped. The dir is created if missing so the path shown in Settings is
/// real.
fn scan_userscript_files() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Some(dir) = userscripts_dir() else {
        return out;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "js"))
        .collect();
    paths.sort(); // deterministic injection order
    for p in paths {
        if let Ok(src) = std::fs::read_to_string(&p) {
            out.push((p, src));
        }
    }
    out
}

/// Build the add-on registry: load persisted consent state, scan the userscripts dir, parse
/// Greasemonkey metadata, and reconcile (design §3 `discover → parse → diff → persist`).
///
/// Reconciliation rules:
/// * New script id → inserted disabled, `granted` empty (awaits the consent dialog).
/// * Known id, content_hash unchanged → keep persisted `enabled`/`granted` as-is (silent).
/// * Known id, content changed → re-parse metadata; keep `enabled` only if the new requested
///   permission set did **not** grow vs. the old grant (design §3 "no silent privilege creep"),
///   otherwise force back to disabled-pending-consent. `granted` is clamped to the new
///   `requested` set.
/// * Bare legacy `*.js` with no metadata block → treated as a trusted, all-sites, no-grant
///   add-on (back-compat), enabled by default.
///
/// Resilient: a missing dir / missing or malformed registry file yields a best-effort registry
/// rather than failing startup.
fn load_addons() -> userscripts::Registry {
    let mut reg = addons_path()
        .map(userscripts::Registry::load)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_else(userscripts::Registry::new);

    for (path, src) in scan_userscript_files() {
        let hash = userscripts::content_hash(&src);
        let is_legacy_bare = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.ends_with(".user.js"));

        let meta = userscripts::parse_metadata(&src);
        // A legacy bare *.js with no metadata: synthesize an all-sites, no-grant add-on.
        let meta = match (meta, is_legacy_bare) {
            (Some(m), _) => m,
            (None, true) => userscripts::Metadata {
                name: Some(
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("userscript")
                        .to_string(),
                ),
                includes: vec!["*".to_string()],
                ..Default::default()
            },
            // A *.user.js without a metadata block is malformed; skip it.
            (None, false) => continue,
        };

        let fresh = userscripts::Addon::from_metadata(&meta, &path, hash);
        match reg.get(&fresh.id).cloned() {
            None => {
                let mut a = fresh;
                // Legacy bare scripts are trusted/back-compat: enable on first sight.
                if is_legacy_bare {
                    a.enabled = true;
                }
                reg.upsert(a);
            }
            Some(prev) => {
                let prev_hash = match &prev.source {
                    userscripts::AddonSource::Userscript { content_hash, .. } => *content_hash,
                };
                let mut a = fresh;
                if prev_hash == hash {
                    // Unchanged code: keep prior consent verbatim.
                    a.enabled = prev.enabled;
                    a.granted = prev.granted;
                } else {
                    // Code changed: clamp granted to the (possibly different) requested set, and
                    // re-prompt (disable) if the requested permissions grew beyond the old grant.
                    let mut granted = userscripts::PermissionSet::new();
                    for p in prev.granted.iter() {
                        if a.requested.contains(p) {
                            granted.insert(p);
                        }
                    }
                    let grew = !a.requested.is_subset(&granted);
                    if grew {
                        // Requested permissions grew beyond the prior grant — must re-consent.
                        // CLEAR the grant entirely (not just disable) so `prompt_pending_consents`
                        // re-queues it via its `granted.is_empty()` filter; otherwise the script
                        // would be silently disabled with a stale partial grant and never
                        // re-surfaced (design §3).
                        a.granted = userscripts::PermissionSet::new();
                        a.enabled = false;
                    } else {
                        a.granted = granted;
                        a.enabled = prev.enabled;
                    }
                }
                reg.upsert(a);
            }
        }
    }

    // Drop registry entries whose backing file disappeared.
    let on_disk: std::collections::HashSet<userscripts::AddonId> = scan_userscript_files()
        .into_iter()
        .filter_map(|(path, src)| {
            userscripts::parse_metadata(&src)
                .map(|m| userscripts::AddonId::for_userscript(&m, &path))
                .or_else(|| {
                    let is_legacy_bare = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| !n.ends_with(".user.js"));
                    if is_legacy_bare {
                        let m = userscripts::Metadata {
                            name: Some(
                                path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("userscript")
                                    .to_string(),
                            ),
                            includes: vec!["*".to_string()],
                            ..Default::default()
                        };
                        Some(userscripts::AddonId::for_userscript(&m, &path))
                    } else {
                        None
                    }
                })
        })
        .collect();
    reg.addons.retain(|a| on_disk.contains(&a.id));

    if let Some(p) = addons_path() {
        let _ = reg.save(p);
    }
    reg
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

/// A stable hue (0..360) for a tab's generated favicon square, derived from its title/url
/// via FNV-1a — so a given site always gets the same color.
fn favicon_hue(s: &str) -> f32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    (h % 360) as f32
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

/// A row the scoped omnibar launcher can select: switch to an open tab, or load a URL.
enum OmniPick {
    Tab(usize),
    Url(String),
}

/// Scoped omnibar verbs: a leading `t `/`b ` (tab / bookmark, space-delimited so `t.co` isn't a
/// verb) or `/` (find-in-page) selects a search scope. Returns `(verb, rest-of-query)`.
fn omnibar_verb(s: &str) -> Option<(char, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('/') {
        return Some(('/', rest));
    }
    let mut it = s.char_indices();
    let (_, first) = it.next()?;
    let v = first.to_ascii_lowercase();
    if matches!(v, 't' | 'b') {
        if let Some((i, c)) = it.next() {
            if c == ' ' {
                return Some((v, s[i..].trim_start()));
            }
        }
    }
    None
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


/// Whether to *additionally* OS-confine content processes (gaol: user-namespace + chroot +
/// seccomp) on top of multiprocess isolation. gaol 0.2.1 **hard-panics the constellation** when
/// the host denies unprivileged namespace creation — which happens in many containers and under
/// AppArmor *even when* the `unprivileged_userns_clone` / `max_user_namespaces` sysctls read OK
/// (observed: EPERM on `unshare` despite both gates open) — and the failure is unrecoverable.
/// So the OS sandbox is **opt-in** (`NAVGATOR_SANDBOX=1`, Linux x86-64 only). Multiprocess
/// process-isolation — the larger security win, and always safe — stays on regardless.
fn sandbox_enabled() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && std::env::var_os("NAVGATOR_SANDBOX").is_some()
}

/// `--sandbox-selftest`: the deterministic, headless confinement gate (plan §8.1, §13.5).
///
/// Runs in a *real* process (not a `gator://` probe page, which would execute in the broker and
/// falsely pass). It applies the **same** policy production uses — `apply_sandbox(
/// &content_process_policy())`, the identical builders `create_sandbox()` calls in the engine — so
/// there is no policy drift to test against. Then it runs the negative-capability battery and exits
/// 0 iff every *hard* gate denies; otherwise 1 with a per-op report.
///
/// Hard gates: unauthorized file read (Landlock), TCP connect (Landlock), and AF_INET/UDP
/// socket creation (seccomp Errno-enforce restricts socket() to AF_UNIX).
/// Informational: process exec — execve is intentionally allowed (gst-plugin-scanner spawn,
/// contained by Landlock exec-path + PR_SET_NO_NEW_PRIVS), not denied.
fn run_sandbox_selftest() -> ! {
    use std::io::Write as _;
    use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
    use std::time::Duration;

    eprintln!("navgator --sandbox-selftest: applying production content-process policy");

    // 1) Apply the EXACT production policy (same builders create_sandbox() uses).
    let outcome: SandboxOutcome = apply_sandbox(&content_process_policy());
    eprintln!("navgator --sandbox-selftest: sandbox outcome = {outcome:?}");

    // The Landlock+seccomp backend is Linux x86-64 only; elsewhere apply_sandbox is a no-op stub
    // (macOS/Windows OS confinement, if any, flows through a different backend). With nothing
    // confined the negative-capability battery below would falsely "leak", so skip it cleanly and
    // exit 0 rather than reporting a confusing failure. (`cfg!` keeps the battery + its imports
    // compiled on every platform — no unused-import / unreachable warnings.)
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        eprintln!(
            "navgator --sandbox-selftest: no Landlock+seccomp backend on this platform \
             (Linux x86-64 only) — nothing to assert; skipping."
        );
        std::process::exit(0);
    }

    struct Probe {
        name: &'static str,
        hard: bool,
        denied: bool,
        detail: String,
    }
    let mut probes: Vec<Probe> = Vec::new();

    // 2a) Unauthorized file reads -> expect Err (EACCES/EPERM under Landlock). HARD gate.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    for path in [format!("{home}/.ssh/id_rsa"), "/etc/shadow".to_string()] {
        let r = std::fs::File::open(&path);
        let denied = r.is_err();
        probes.push(Probe {
            name: "file-read",
            hard: true,
            denied,
            detail: match &r {
                Ok(_) => format!("{path}: OPENED (leak!)"),
                Err(e) => format!("{path}: {e}"),
            },
        });
    }

    // 2b) TCP connect -> expect Err (Landlock AccessNet::ConnectTcp handled, no allow rule). HARD.
    {
        let target = "1.1.1.1:80";
        let result = target
            .to_socket_addrs()
            .map_err(|e| e.to_string())
            .and_then(|mut addrs| {
                addrs
                    .next()
                    .ok_or_else(|| "no addr".to_string())
                    .and_then(|addr| {
                        TcpStream::connect_timeout(&addr, Duration::from_millis(800))
                            .map_err(|e| e.to_string())
                    })
            });
        let denied = result.is_err();
        probes.push(Probe {
            name: "tcp-connect",
            hard: true,
            denied,
            detail: match &result {
                Ok(_) => format!("{target}: CONNECTED (leak!)"),
                Err(e) => format!("{target}: {e}"),
            },
        });
    }

    // 2b-bis) INET socket *creation* -> expect Err (EPERM). HARD gate: proves seccomp
    // restricts socket() to AF_UNIX, so content cannot even CREATE an AF_INET/UDP/raw
    // socket (Landlock's AccessNet only denies TCP bind/connect, not the socket() call
    // nor UDP/raw). UdpSocket::bind() does socket(AF_INET, SOCK_DGRAM) under the hood,
    // which must now EPERM at the socket() syscall before any bind is attempted.
    {
        let result = UdpSocket::bind("127.0.0.1:0");
        let denied = result.is_err();
        probes.push(Probe {
            name: "inet-socket",
            hard: true,
            denied,
            detail: match &result {
                Ok(_) => "UDP/AF_INET socket: CREATED (leak! socket() not AF_UNIX-restricted)"
                    .to_string(),
                Err(e) => format!("UDP/AF_INET bind: {e}"),
            },
        });
    }

    // 2c) Process exec -> INFORMATIONAL: execve is intentionally allowed (gst-plugin-scanner
    //     spawn), contained by Landlock exec-path + PR_SET_NO_NEW_PRIVS, not seccomp-denied.
    {
        let result = std::process::Command::new("/bin/true").spawn();
        let denied = result.is_err();
        if let Ok(mut child) = result {
            let _ = child.wait();
        }
        probes.push(Probe {
            name: "process-exec",
            hard: false,
            denied,
            detail: "/bin/true spawn (informational until seccomp enforce)".to_string(),
        });
    }

    // 3) Report.
    let stderr = std::io::stderr();
    let mut w = stderr.lock();
    let _ = writeln!(w, "\n=== navgator sandbox selftest ===");
    let _ = writeln!(w, "sandbox outcome: {outcome:?}\n");
    let mut all_hard_denied = true;
    for p in &probes {
        let verdict = if p.denied { "DENIED" } else { "ALLOWED" };
        let tag = if p.hard {
            if !p.denied {
                all_hard_denied = false;
            }
            "[gate]"
        } else {
            "[info]"
        };
        let _ = writeln!(w, "  {tag:<6} {:<13} -> {verdict}   ({})", p.name, p.detail);
    }
    let _ = writeln!(
        w,
        "\nresult: {}",
        if all_hard_denied {
            "PASS (all hard gates denied)"
        } else {
            "FAIL (a forbidden op was ALLOWED)"
        }
    );
    let _ = w.flush();

    std::process::exit(if all_hard_denied { 0 } else { 1 });
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
    // MSE: adaptive-streaming DOM API — single + multi-segment VP8/WebM play (LYK-1361).
    p.dom_mediasource_enabled = true;
    // EME Clear Key: CENC decrypt element + `encrypted` event — encrypted H.264 plays end to
    // end (LYK-1429). Clear Key only; Widevine/PlayReady are rejected (NotSupported).
    p.dom_eme_enabled = true;
    // Second wave — features with real implementations in the fork (additive, low-risk).
    p.dom_offscreen_canvas_enabled = true; // OffscreenCanvas (2d/bitmap/webgl)
    p.dom_sanitizer_enabled = true; // HTML Sanitizer API (security pitch)
    p.dom_exec_command_enabled = true; // contenteditable rich-text editing
    p.dom_storage_manager_api_enabled = true; // navigator.storage
    // Enablement audit (LYK-1383): default-OFF stock prefs whose swervo impls are complete +
    // additive. `cookiestore.rs` is a full 746-line Get/Set/Delete impl over the cookie jar;
    // `wakelock.rs` is a spec-compliant Screen Wake Lock (permission-gated, rejects cleanly when
    // denied). (Excluded: webvtt — vttcue::GetCueAsHTML is a `todo!()` panic; sharedworker/
    // abort_controller/resize_observer/mutation_observer/crypto_subtle are already default-on.)
    p.dom_cookiestore_enabled = true; // CookieStore API (async cookie read/write)
    p.dom_wakelock_enabled = true; // Screen Wake Lock (navigator.wakeLock.request)
    // CSS Masking: stylo gates mask-* parsing behind `layout.unimplemented`. swervo now paints
    // mask-image (single-layer alpha mask → display_list mask clip), so opt in to parsing it.
    // The other layout.unimplemented-gated properties remain parse-only no-ops.
    p.layout_unimplemented = true; // CSS `mask-image` (monochrome icons: MDN chevrons, etc.)
    // CSS Grid: swervo lays out grid via taffy, but stylo gates `display:grid` parsing behind
    // `layout.grid.enabled` (default off) — so without this, `display:grid` is dropped as invalid
    // and the container falls back to `block` (items collapse to a full-width 1-column stack).
    // The grid layout path itself is fully implemented; this just opts the parser in.
    p.layout_grid_enabled = true; // CSS Grid (marketing-site hero/card layouts, etc.)
    // More stylo parse-gates that default OFF but are implemented in swervo — additive standards
    // features (sites that don't use them are unaffected; sites that do now render correctly
    // instead of dropping the property). Gap analysis LYK-1362.
    p.layout_container_queries_enabled = true; // `@container` (ubiquitous on modern responsive sites)
    p.layout_columns_enabled = true; // CSS multi-column (`column-count`/`column-width`)
    p.layout_variable_fonts_enabled = true; // variable fonts (weight/width axes)
    p.layout_writing_mode_enabled = true; // `writing-mode: vertical-*` (CJK + vertical layouts)
    // (Font parity handled in the engine: swervo resolves uninstalled named families via
    // fontconfig like Chrome — Arial->Liberation, Verdana->Noto, sans-serif->DejaVu — so no pref
    // override is needed. See airgap/swervo font_list font_family_substitute.)
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

/// A single setting change parsed from a `gator://settings?key=value` link. Each rendered link
/// carries exactly one param, so `load_web_resource` produces exactly one of these per load and
/// `render_gator_settings` applies it. `Action` carries an imperative button (sync/import/default).
enum SettingsApply {
    None,
    Engine(usize),
    Search(String),
    Theme(usize),
    Accent(String),
    Dark(bool),
    BlockAds(bool),
    SyncBookmarks(bool),
    SyncHistory(bool),
    SyncPasswords(bool),
    /// A rich-theme change from the Appearance section: (key, value), applied to `Settings::theme`
    /// (key ∈ base/accentk/density/font/tabpos/tabfit/wallpaper/preset/radius/glass/tabmaxw/module).
    ThemeSet(String, String),
    Action(String),
}

/// A `gator://settings` toggle rendered as an iOS-style switch that links to the OPPOSITE value.
/// `section` is the settings hash (`privacy`/`sync`/…) so a toggle keeps its page open on reload.
/// Shared by the block_ads/sync_* boolean settings on the in-page settings UI.
fn toggle_link(key: &str, on: bool, section: &str) -> String {
    let next = if on { "off" } else { "on" };
    format!(
        "<a class=\"tog{}\" href=\"gator://settings?{}={}#{}\"><span class=\"knob\"></span></a>",
        if on { " on" } else { "" },
        key,
        next,
        section,
    )
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

/// Encode a decoded favicon to PNG (straight RGBA8) for the on-disk favicon cache (#14), so the
/// new-tab tiles can render from cache instead of a live `/favicon.ico` fetch per open.
fn favicon_to_png(image: &Image) -> Option<Vec<u8>> {
    let (w, h) = (image.width, image.height);
    if w == 0 || h == 0 {
        return None;
    }
    let d = image.data();
    let rgba: Vec<u8> = match image.format {
        PixelFormat::K8 => d.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        PixelFormat::KA8 => d.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        PixelFormat::RGB8 => d.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        PixelFormat::RGBA8 => d.to_vec(),
        PixelFormat::BGRA8 => d.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect(),
    };
    if rgba.len() != w as usize * h as usize * 4 {
        return None;
    }
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;
    writer.finish().ok()?;
    Some(buf)
}

/// Path for a host-keyed cached favicon. Sanitizes the host (alphanumerics / `.` / `-` only) so a
/// crafted gator://favicon/<host> can't escape the cache dir.
fn favicon_cache_path(host: &str) -> Option<std::path::PathBuf> {
    if host.is_empty()
        || host.len() > 100
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    config_file(&format!("favicons/{host}.png"))
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

/// Map a swervo CSS `cursor` keyword to the winit window cursor. The variant names match
/// `cursor-icon` one-to-one (both follow the CSS `cursor` set); `None` (`cursor: none`) has no
/// winit icon, so it falls back to the default arrow rather than hiding the pointer.
fn map_cursor(c: Cursor) -> CursorIcon {
    match c {
        Cursor::None | Cursor::Default => CursorIcon::Default,
        Cursor::Pointer => CursorIcon::Pointer,
        Cursor::ContextMenu => CursorIcon::ContextMenu,
        Cursor::Help => CursorIcon::Help,
        Cursor::Progress => CursorIcon::Progress,
        Cursor::Wait => CursorIcon::Wait,
        Cursor::Cell => CursorIcon::Cell,
        Cursor::Crosshair => CursorIcon::Crosshair,
        Cursor::Text => CursorIcon::Text,
        Cursor::VerticalText => CursorIcon::VerticalText,
        Cursor::Alias => CursorIcon::Alias,
        Cursor::Copy => CursorIcon::Copy,
        Cursor::Move => CursorIcon::Move,
        Cursor::NoDrop => CursorIcon::NoDrop,
        Cursor::NotAllowed => CursorIcon::NotAllowed,
        Cursor::Grab => CursorIcon::Grab,
        Cursor::Grabbing => CursorIcon::Grabbing,
        Cursor::EResize => CursorIcon::EResize,
        Cursor::NResize => CursorIcon::NResize,
        Cursor::NeResize => CursorIcon::NeResize,
        Cursor::NwResize => CursorIcon::NwResize,
        Cursor::SResize => CursorIcon::SResize,
        Cursor::SeResize => CursorIcon::SeResize,
        Cursor::SwResize => CursorIcon::SwResize,
        Cursor::WResize => CursorIcon::WResize,
        Cursor::EwResize => CursorIcon::EwResize,
        Cursor::NsResize => CursorIcon::NsResize,
        Cursor::NeswResize => CursorIcon::NeswResize,
        Cursor::NwseResize => CursorIcon::NwseResize,
        Cursor::ColResize => CursorIcon::ColResize,
        Cursor::RowResize => CursorIcon::RowResize,
        Cursor::AllScroll => CursorIcon::AllScroll,
        Cursor::ZoomIn => CursorIcon::ZoomIn,
        Cursor::ZoomOut => CursorIcon::ZoomOut,
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

/// Force-dark: invert the whole page + hue-rotate to keep hues, then re-invert media so photos
/// look normal. Crude but universal; idempotent (keyed by the style id).
const FORCE_DARK_JS: &str = r#"(function(){var id='__ng_forcedark';if(document.getElementById(id))return;var s=document.createElement('style');s.id=id;s.textContent='html{background:#0b0b0d!important;filter:invert(1) hue-rotate(180deg)}img,video,picture,canvas,svg,iframe,embed,object,[style*="background-image"]{filter:invert(1) hue-rotate(180deg)}';(document.head||document.documentElement).appendChild(s);})()"#;

/// Remove the force-dark style injected by `FORCE_DARK_JS`.
const FORCE_DARK_OFF_JS: &str = r#"(function(){var e=document.getElementById('__ng_forcedark');if(e)e.remove();})()"#;

/// Reader mode: a Readability-lite that extracts the main article in-place and restyles the page
/// into a clean, centered, dark reading column. Injected fire-and-forget; reload to exit.
const READER_JS: &str = r#"(function(){try{
var pick=document.querySelector('article')||document.querySelector('[role=main]')||document.querySelector('main');
if(!pick){var best=null,score=0;document.querySelectorAll('div,section').forEach(function(el){var ps=el.querySelectorAll('p');if(ps.length<3)return;var len=(el.innerText||'').length;if(len>score){score=len;best=el;}});pick=best||document.body;}
var title=((document.querySelector('h1')||{}).innerText||document.title||'').trim();
var clone=pick.cloneNode(true);
clone.querySelectorAll('script,style,nav,aside,header,footer,form,iframe,button,h1,.ad,[data-ad],.advertisement,.comments,.social,.share,.newsletter').forEach(function(n){n.remove();});
document.head.querySelectorAll('style,link[rel="stylesheet"]').forEach(function(n){n.remove();});
document.body.innerHTML='<div id="ngreader"><h1>'+title.replace(/</g,'&lt;')+'</h1>'+clone.innerHTML+'</div>';
var s=document.createElement('style');
s.textContent='html,body{margin:0;background:#111418;color:#e6e8eb}#ngreader{max-width:44rem;margin:48px auto 96px;padding:0 24px;font:19px/1.75 Georgia,serif}#ngreader h1{font:600 34px/1.2 system-ui,sans-serif;margin:0 0 24px;color:#fff}#ngreader img{max-width:100%;height:auto;border-radius:6px}#ngreader a{color:#7aa2ff}#ngreader p{margin:0 0 20px}#ngreader pre{overflow:auto;background:#0b0d10;padding:12px;border-radius:6px}';
document.head.appendChild(s);
}catch(e){console.log('reader error '+e);}})()"#;

/// DOM-render diagnostic, injected only when `NAVGATOR_DOMPROBE` is set. Snapshots DOM size +
/// laid-out box count at load, +3s and +6s, logging to the console (captured with
/// `NAVGATOR_CONSOLE`). Distinguishes a silent CSR hydration failure's two modes: DOM populated
/// but no visible boxes (layout/paint gap) vs DOM never grows (JS never mounted).
const DOM_PROBE_JS: &str = r#"(function(){try{var _o=XMLHttpRequest.prototype.open,_s=XMLHttpRequest.prototype.send;XMLHttpRequest.prototype.open=function(m,u,a){this.__s=(a===false);this.__u=u;if(a===false)console.log('NGBLOCK syncXHR.open '+m+' '+u);return _o.apply(this,arguments);};XMLHttpRequest.prototype.send=function(){if(this.__s)console.log('NGBLOCK syncXHR.send '+this.__u);var r=_s.apply(this,arguments);if(this.__s)console.log('NGBLOCK syncXHR.done '+this.__u+' status='+this.status);return r;};['alert','confirm','prompt'].forEach(function(f){var _f=window[f];window[f]=function(){console.log('NGBLOCK '+f+' '+Array.prototype.slice.call(arguments).join(','));return f==='confirm'?true:(f==='prompt'?null:undefined);};});}catch(e){console.log('NGBLOCK instr-err '+e);}function snap(t){try{var b=document.body;var all=document.querySelectorAll('body *');var vis=0;for(var i=0;i<all.length;i++){var r=all[i].getBoundingClientRect();if(r.width>1&&r.height>1)vis++;}console.log('NGPROBE t='+t+' nodes='+document.querySelectorAll('*').length+' bodyTextLen='+(b?b.innerText.length:-1)+' visibleBoxes='+vis+' bodyChildren='+(b?b.children.length:-1)+' bodyClientH='+(b?b.clientHeight:-1)+' ready='+document.readyState);}catch(e){console.log('NGPROBE err '+e);}}snap(0);setTimeout(function(){snap(3000);},3000);setTimeout(function(){snap(6000);},6000);})()"#;

/// Credential Firewall (#19): on a password/card field focus, ping the native side with this
/// page's origin/host so it can warn if the site is a look-alike of one with a saved login.
/// Advisory only (runs in the page's world); the bridge fetch is intercepted by load_web_resource.
// Two Servo realities, both confirmed on a live /run, shaped this:
//   1. Delivery is an Image beacon, NOT fetch() — Servo's Fetch API does not route custom
//      (navgator://) schemes to the WebResourceLoad interceptor, but a subresource load (img, like
//      gator://font/*) does. The "image" load fails harmlessly; the request reaching
//      load_web_resource is the point. Fire-and-forget: native shows the toast, the page reads
//      nothing back.
//   2. The trigger is click/keydown, NOT focusin — Servo doesn't reliably dispatch focus events.
// Fires at most once per document (the warning is about the site, not each keystroke).
/// Web Animations API polyfill (LYK-1411). swervo ships the WAAPI DOM interfaces as stubs
/// (`Element.animate` returns a dead object; no `getAnimations`/playback/interpolation), so
/// `element.animate(...)` never drives style. This injects a compact, spec-shaped polyfill at
/// document-start on every page — it self-gates off when the engine's WAAPI is complete.
/// requestIdleCallback / cancelIdleCallback polyfill (LYK-1403). swervo exposes neither;
/// this schedules the callback past the current turn (and a frame when available) with an
/// IdleDeadline shim, honoring the `timeout` option. Injected with the WAAPI polyfill.
/// Platform polyfills for a couple of commonly-used web APIs swervo lacks (found via a
/// feature-detect scan): `scheduler.postTask`/`scheduler.yield` (used by React/Next
/// schedulers) and `Element.checkVisibility`. Each self-gates off if native. Injected
/// with the WAAPI + rIC polyfills at document-start.
const PLATFORM_POLYFILLS_JS: &str = r#"(function () {
"use strict";
try {
if (typeof self.scheduler === "undefined" || typeof self.scheduler.postTask !== "function") {
var chan = typeof MessageChannel === "function" ? new MessageChannel() : null;
var flushQueued = false;
var pumps = [];
if (chan) chan.port1.onmessage = function () { var f = pumps.shift(); if (f) f(); };
function nextTick(fn) { if (chan) { pumps.push(fn); chan.port2.postMessage(0); } else setTimeout(fn, 0); }
var PRIOS = ["user-blocking", "user-visible", "background"];
var queues = { "user-blocking": [], "user-visible": [], background: [] };
function schedule() {
if (flushQueued) return;
flushQueued = true;
nextTick(dispatch);
}
function dispatch() {
flushQueued = false;
for (var i = 0; i < PRIOS.length; i++) {
var q = queues[PRIOS[i]];
if (q.length) { var task = q.shift(); task(); break; }
}
for (var j = 0; j < PRIOS.length; j++) if (queues[PRIOS[j]].length) { schedule(); break; }
}
var scheduler = self.scheduler || {};
scheduler.postTask = function (callback, options) {
options = options || {};
var priority = queues[options.priority] ? options.priority : "user-visible";
var delay = options.delay > 0 ? options.delay : 0;
var signal = options.signal;
return new Promise(function (resolve, reject) {
if (signal && signal.aborted) {
reject(signal.reason || new DOMException("Aborted", "AbortError"));
return;
}
var ran = false;
function run() {
if (ran) return; ran = true;
if (signal) try { signal.removeEventListener("abort", onAbort); } catch (e) {}
try { resolve(callback()); } catch (e) { reject(e); }
}
function onAbort() {
if (ran) return; ran = true;
var idx = queues[priority].indexOf(run);
if (idx >= 0) queues[priority].splice(idx, 1);
reject(signal.reason || new DOMException("Aborted", "AbortError"));
}
if (signal) try { signal.addEventListener("abort", onAbort); } catch (e) {}
function enqueue() { queues[priority].push(run); schedule(); }
if (delay > 0) setTimeout(enqueue, delay); else enqueue();
});
};
if (typeof scheduler.yield !== "function") {
scheduler.yield = function () {
return new Promise(function (resolve) {
queues["user-visible"].push(resolve); schedule();
});
};
}
self.scheduler = scheduler;
}
} catch (e) { try { console.log("NGPLAT scheduler err " + e); } catch (e2) {} }
try {
if (typeof Element.prototype.checkVisibility !== "function") {
Element.prototype.checkVisibility = function (options) {
options = options || {};
var el = this;
if (!el.isConnected) return false;
var node = el;
while (node && node.nodeType === 1) {
var cs;
try { cs = getComputedStyle(node); } catch (e) { return false; }
if (!cs) break;
if (cs.display === "none") return false;
if (cs.contentVisibility === "hidden") return false;
if ((options.visibilityProperty || options.checkVisibilityCSS) &&
(cs.visibility === "hidden" || cs.visibility === "collapse")) return false;
if ((options.opacityProperty || options.checkOpacity) && parseFloat(cs.opacity) === 0) return false;
node = node.parentElement;
}
if (el.getClientRects && el.getClientRects().length === 0) return false;
return true;
};
}
} catch (e) { try { console.log("NGPLAT checkVisibility err " + e); } catch (e2) {} }
try {
if (typeof window.ServiceWorkerRegistration === "undefined")
window.ServiceWorkerRegistration = function ServiceWorkerRegistration() {};
} catch (e) { try { console.log("NGPLAT swstub err " + e); } catch (e2) {} }
})();"#;

const RIC_POLYFILL_JS: &str = r#"(function () {
"use strict";
if (typeof window.requestIdleCallback === "function" &&
typeof window.cancelIdleCallback === "function") return;
if (window.__ngRic) return;
window.__ngRic = 1;
var now = function () {
return (window.performance && performance.now && performance.now()) || Date.now();
};
var pending = {};
var nextId = 1;
var FRAME_BUDGET = 50; // ms, per the spec's suggested cap for timeRemaining()
window.requestIdleCallback = function (callback, options) {
var id = nextId++;
var timeoutMs = options && typeof options.timeout === "number" ? options.timeout : 0;
var scheduledAt = now();
var entry = { cb: callback, soon: 0, hard: 0, done: false };
function fire(didTimeout) {
if (entry.done || !pending[id]) return;
entry.done = true;
if (entry.soon) clearTimeout(entry.soon);
if (entry.hard) clearTimeout(entry.hard);
delete pending[id];
var startRun = now();
callback({
didTimeout: !!didTimeout,
timeRemaining: function () {
return Math.max(0, FRAME_BUDGET - (now() - startRun));
},
});
}
entry.soon = setTimeout(function () {
var raf = window.requestAnimationFrame;
if (raf) raf(function () { setTimeout(function () { fire(false); }, 0); });
else fire(false);
}, 1);
if (timeoutMs > 0) {
entry.hard = setTimeout(function () { fire(true); }, timeoutMs);
}
pending[id] = entry;
return id;
};
window.cancelIdleCallback = function (id) {
var e = pending[id];
if (e) {
if (e.soon) clearTimeout(e.soon);
if (e.hard) clearTimeout(e.hard);
delete pending[id];
}
};
})();"#;

const WAAPI_POLYFILL_JS: &str = r#"(function () {
"use strict";
try {
if (
typeof Animation === "function" &&
typeof Animation.prototype.play === "function" &&
typeof Element.prototype.getAnimations === "function" &&
typeof Animation.prototype.finished !== "undefined"
) {
return; // native WAAPI is functional; leave it alone.
}
} catch (e) {}
if (window.__ngWaapi) return;
window.__ngWaapi = 1;
var raf =
window.requestAnimationFrame ||
function (cb) {
return setTimeout(function () {
cb(Date.now());
}, 16);
};
function cubicBezier(p1x, p1y, p2x, p2y) {
var cx = 3 * p1x,
bx = 3 * (p2x - p1x) - cx,
ax = 1 - cx - bx;
var cy = 3 * p1y,
by = 3 * (p2y - p1y) - cy,
ay = 1 - cy - by;
function fx(t) {
return ((ax * t + bx) * t + cx) * t;
}
function dfx(t) {
return (3 * ax * t + 2 * bx) * t + cx;
}
return function (x) {
if (x <= 0) return 0;
if (x >= 1) return 1;
var t = x;
for (var i = 0; i < 8; i++) {
var e = fx(t) - x;
if (Math.abs(e) < 1e-4) break;
var d = dfx(t);
if (Math.abs(d) < 1e-6) break;
t -= e / d;
}
return ((ay * t + by) * t + cy) * t;
};
}
var EASINGS = {
linear: function (t) {
return t;
},
ease: cubicBezier(0.25, 0.1, 0.25, 1),
"ease-in": cubicBezier(0.42, 0, 1, 1),
"ease-out": cubicBezier(0, 0, 0.58, 1),
"ease-in-out": cubicBezier(0.42, 0, 0.58, 1),
"step-start": function (t) {
return t > 0 ? 1 : 0;
},
"step-end": function (t) {
return t >= 1 ? 1 : 0;
},
};
function parseEasing(str) {
if (!str || str === "linear") return EASINGS.linear;
if (EASINGS[str]) return EASINGS[str];
var m = /cubic-bezier\(([^)]+)\)/.exec(str);
if (m) {
var a = m[1].split(",").map(parseFloat);
return cubicBezier(a[0], a[1], a[2], a[3]);
}
m = /steps\(\s*(\d+)\s*(?:,\s*(\w+))?\s*\)/.exec(str);
if (m) {
var n = parseInt(m[1], 10),
pos = m[2] || "end";
return function (t) {
if (t >= 1) return 1;
if (t <= 0) return 0;
var step = Math.floor(t * n);
if (pos === "start" || pos === "jump-start") step += 1;
return Math.min(1, step / n);
};
}
return EASINGS.linear;
}
function parseColor(s) {
s = s.trim();
var m = /^#([0-9a-f]{3,8})$/i.exec(s);
if (m) {
var h = m[1];
if (h.length === 3)
h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2];
if (h.length === 4)
h = h[0]+h[0]+h[1]+h[1]+h[2]+h[2]+h[3]+h[3];
return [
parseInt(h.slice(0, 2), 16),
parseInt(h.slice(2, 4), 16),
parseInt(h.slice(4, 6), 16),
h.length >= 8 ? parseInt(h.slice(6, 8), 16) / 255 : 1,
];
}
m = /^rgba?\(([^)]+)\)$/i.exec(s);
if (m) {
var p = m[1].split(/[,\/\s]+/).filter(Boolean).map(parseFloat);
return [p[0], p[1], p[2], p.length > 3 ? p[3] : 1];
}
return null;
}
function lerp(a, b, t) {
return a + (b - a) * t;
}
function makeInterp(from, to) {
from = String(from).trim();
to = String(to).trim();
if (/^-?[\d.]+$/.test(from) && /^-?[\d.]+$/.test(to)) {
var a = parseFloat(from),
b = parseFloat(to);
return function (t) {
return String(lerp(a, b, t));
};
}
var cf = parseColor(from),
ct = parseColor(to);
if (cf && ct) {
return function (t) {
return (
"rgba(" +
Math.round(lerp(cf[0], ct[0], t)) +
"," +
Math.round(lerp(cf[1], ct[1], t)) +
"," +
Math.round(lerp(cf[2], ct[2], t)) +
"," +
lerp(cf[3], ct[3], t) +
")"
);
};
}
var numRe = /-?[\d.]+/g;
var skelFrom = from.replace(numRe, " ");
var skelTo = to.replace(numRe, " ");
if (skelFrom === skelTo) {
var nf = from.match(numRe) || [];
var nt = to.match(numRe) || [];
if (nf.length === nt.length && nf.length > 0) {
var fa = nf.map(parseFloat),
ta = nt.map(parseFloat);
var parts = from.split(numRe); // literal segments around numbers
return function (t) {
var out = parts[0];
for (var i = 0; i < fa.length; i++) {
out += fmtNum(lerp(fa[i], ta[i], t)) + parts[i + 1];
}
return out;
};
}
}
return null; // discrete
}
function fmtNum(n) {
return Math.abs(n) < 1e-6 ? "0" : parseFloat(n.toFixed(4)).toString();
}
var SPECIAL = { offset: 1, easing: 1, composite: 1 };
function normalizeKeyframes(input) {
if (input == null) return [];
var frames = [];
if (Array.isArray(input)) {
frames = input.map(function (k) {
var o = {};
for (var p in k) o[p] = k[p];
return o;
});
} else {
var props = Object.keys(input);
var maxLen = 1;
props.forEach(function (p) {
if (!SPECIAL[p] && Array.isArray(input[p]))
maxLen = Math.max(maxLen, input[p].length);
});
for (var i = 0; i < maxLen; i++) frames.push({});
props.forEach(function (p) {
var v = input[p];
if (SPECIAL[p]) {
for (var i = 0; i < maxLen; i++)
frames[i][p] = Array.isArray(v)
? v[Math.min(i, v.length - 1)]
: v;
} else {
var arr = Array.isArray(v) ? v : [v];
for (var j = 0; j < maxLen; j++)
frames[j][p] = arr[Math.min(j, arr.length - 1)];
}
});
}
var n = frames.length;
if (n) {
if (frames[0].offset == null) frames[0].offset = 0;
if (frames[n - 1].offset == null) frames[n - 1].offset = 1;
}
var lastIdx = 0,
lastOff = frames.length ? frames[0].offset || 0 : 0;
for (var i = 1; i < n; i++) {
if (frames[i].offset != null) {
var span = frames[i].offset - lastOff;
for (var k = lastIdx + 1; k < i; k++)
frames[k].offset = lastOff + (span * (k - lastIdx)) / (i - lastIdx);
lastIdx = i;
lastOff = frames[i].offset;
}
}
return frames;
}
function buildTracks(frames, defaultEasing) {
var tracks = {};
frames.forEach(function (f) {
var ease = f.easing || defaultEasing;
for (var p in f) {
if (SPECIAL[p]) continue;
(tracks[p] = tracks[p] || []).push({
offset: f.offset,
value: f[p],
easing: ease,
});
}
});
for (var p in tracks)
tracks[p].sort(function (a, b) {
return a.offset - b.offset;
});
return tracks;
}
var allAnimations = [];
function normalizeTiming(options) {
var t = {
delay: 0,
endDelay: 0,
duration: 0,
iterations: 1,
iterationStart: 0,
easing: "linear",
direction: "normal",
fill: "none",
};
if (typeof options === "number") {
t.duration = options;
} else if (options && typeof options === "object") {
for (var k in t) if (options[k] != null) t[k] = options[k];
if (options.duration === "auto") t.duration = 0;
}
if (typeof t.duration !== "number" || isNaN(t.duration)) t.duration = 0;
return t;
}
function KEffect(target, keyframes, options) {
this.target = target;
this.timing = normalizeTiming(options);
this._frames = normalizeKeyframes(keyframes);
this._tracks = buildTracks(this._frames, this.timing.easing);
this.id = (options && typeof options === "object" && options.id) || "";
}
KEffect.prototype.getTiming = function () {
return this.timing;
};
KEffect.prototype.getKeyframes = function () {
return this._frames.map(function (f) {
var o = {};
for (var p in f) o[p] = f[p];
return o;
});
};
KEffect.prototype._sample = function (progress, applyBase) {
var el = this.target;
if (!el || !el.style) return;
for (var p in this._tracks) {
var kf = this._tracks[p];
var val;
if (progress <= kf[0].offset) val = kf[0].value;
else if (progress >= kf[kf.length - 1].offset)
val = kf[kf.length - 1].value;
else {
var lo = kf[0];
for (var i = 1; i < kf.length; i++) {
if (progress <= kf[i].offset) {
var hi = kf[i];
var span = hi.offset - lo.offset || 1;
var localT = (progress - lo.offset) / span;
var eased = parseEasing(lo.easing)(localT);
var interp = makeInterp(lo.value, hi.value);
val = interp ? interp(eased) : (eased < 0.5 ? lo.value : hi.value);
break;
}
lo = kf[i];
}
}
setStyleProp(el, p, val);
}
};
KEffect.prototype._clear = function () {
var el = this.target;
if (!el || !el.style) return;
for (var p in this._tracks) setStyleProp(el, p, "");
};
function setStyleProp(el, prop, val) {
var css = prop.replace(/[A-Z]/g, function (m) {
return "-" + m.toLowerCase();
});
try {
el.style.setProperty(css, val === "" ? "" : String(val), "important");
} catch (e) {
try {
el.style[prop] = val;
} catch (e2) {}
}
}
function Anim(effect) {
this.effect = effect;
this._startTime = null;
this._currentTime = 0;
this.playbackRate = 1;
this.playState = "idle";
this._pauseTime = null;
this._finished = null;
this._finishedResolve = null;
this._finishedReject = null;
this.onfinish = null;
this.oncancel = null;
this.id = effect ? effect.id : "";
allAnimations.push(this);
}
Object.defineProperty(Anim.prototype, "currentTime", {
get: function () {
return this._currentTime;
},
set: function (v) {
this._currentTime = v || 0;
if (this.playState === "running")
this._startTime = now() - this._currentTime / this.playbackRate;
this._tick(now(), true);
},
});
Object.defineProperty(Anim.prototype, "finished", {
get: function () {
var self = this;
if (!this._finished) {
this._finished = new Promise(function (res, rej) {
self._finishedResolve = res;
self._finishedReject = rej;
});
if (this.playState === "finished") this._finishedResolve(this);
}
return this._finished;
},
});
Object.defineProperty(Anim.prototype, "ready", {
get: function () {
return Promise.resolve(this);
},
});
function now() {
return (window.performance && performance.now && performance.now()) || Date.now();
}
Anim.prototype._totalDuration = function () {
var t = this.effect.timing;
var iters = t.iterations === Infinity ? Infinity : t.iterations;
return t.delay + t.duration * iters + t.endDelay;
};
Anim.prototype.play = function () {
if (this.playState === "finished") {
if (
this._currentTime >= this.effect.timing.delay +
this.effect.timing.duration * (this.effect.timing.iterations || 1)
)
this._currentTime = 0;
}
this.playState = "running";
this._startTime = now() - this._currentTime / this.playbackRate;
scheduleTick();
return this;
};
Anim.prototype.pause = function () {
this.playState = "paused";
return this;
};
Anim.prototype.reverse = function () {
this.playbackRate = -this.playbackRate;
if (this.playState !== "running") this.play();
else this._startTime = now() - this._currentTime / this.playbackRate;
return this;
};
Anim.prototype.finish = function () {
var t = this.effect.timing;
var end = t.delay + t.duration * (t.iterations === Infinity ? 1 : t.iterations);
this._currentTime = this.playbackRate >= 0 ? end : 0;
this._tick(now(), true);
this._doFinish();
return this;
};
Anim.prototype.cancel = function () {
this.playState = "idle";
this._currentTime = 0;
this.effect._clear();
if (this._finishedReject)
this._finishedReject(mkAbort());
this._finished = null;
if (typeof this.oncancel === "function")
this.oncancel({ type: "cancel", target: this });
dispatch(this, "cancel");
var i = allAnimations.indexOf(this);
if (i >= 0) allAnimations.splice(i, 1);
return this;
};
function mkAbort() {
try {
return new DOMException("The animation was aborted.", "AbortError");
} catch (e) {
var er = new Error("aborted");
er.name = "AbortError";
return er;
}
}
Anim.prototype._doFinish = function () {
if (this.playState === "finished") return;
this.playState = "finished";
if (this._finishedResolve) this._finishedResolve(this);
if (typeof this.onfinish === "function")
this.onfinish({ type: "finish", target: this });
dispatch(this, "finish");
};
Anim.prototype._listeners = null;
Anim.prototype.addEventListener = function (type, fn) {
(this._listeners = this._listeners || {});
(this._listeners[type] = this._listeners[type] || []).push(fn);
};
Anim.prototype.removeEventListener = function (type, fn) {
if (!this._listeners || !this._listeners[type]) return;
var a = this._listeners[type],
i = a.indexOf(fn);
if (i >= 0) a.splice(i, 1);
};
function dispatch(anim, type) {
if (anim._listeners && anim._listeners[type])
anim._listeners[type].slice().forEach(function (fn) {
try {
fn.call(anim, { type: type, target: anim });
} catch (e) {}
});
}
Anim.prototype._tick = function (nowMs, forceSample) {
var t = this.effect.timing;
if (this.playState === "running") {
this._currentTime = (nowMs - this._startTime) * this.playbackRate;
}
var ct = this._currentTime;
var active = ct - t.delay;
var dur = t.duration;
var iterations = t.iterations;
var beforePhase = active < 0;
var totalActive = dur * (iterations === Infinity ? Infinity : iterations);
var afterPhase = iterations !== Infinity && active >= totalActive;
var fill = t.fill;
var fillBackwards = fill === "backwards" || fill === "both";
var fillForwards = fill === "forwards" || fill === "both";
var iterProgress, currentIter;
if (dur <= 0) {
iterProgress = afterPhase ? 1 : 0;
currentIter = 0;
} else if (beforePhase) {
iterProgress = t.iterationStart % 1;
currentIter = Math.floor(t.iterationStart);
} else if (afterPhase) {
var it = iterations;
iterProgress = 1;
currentIter = Math.max(0, Math.ceil(it) - 1);
if (it % 1 === 0) iterProgress = 1;
} else {
var overall = t.iterationStart + active / dur;
currentIter = Math.floor(overall);
iterProgress = overall - currentIter;
}
var dir = t.direction;
var reverse = false;
if (dir === "reverse") reverse = true;
else if (dir === "alternate") reverse = currentIter % 2 === 1;
else if (dir === "alternate-reverse") reverse = currentIter % 2 === 0;
var directed = reverse ? 1 - iterProgress : iterProgress;
var shouldSample =
(!beforePhase && !afterPhase) ||
(beforePhase && fillBackwards) ||
(afterPhase && fillForwards) ||
forceSample;
if (shouldSample) {
this.effect._sample(directed, true);
} else if (beforePhase && !fillBackwards) {
this.effect._clear();
} else if (afterPhase && !fillForwards) {
this.effect._clear();
}
if (afterPhase && this.playState === "running") {
this._doFinish();
}
};
var ticking = false;
function scheduleTick() {
if (ticking) return;
ticking = true;
raf(tickAll);
}
function tickAll() {
ticking = false;
var nowMs = now();
var anyRunning = false;
for (var i = 0; i < allAnimations.length; i++) {
var a = allAnimations[i];
if (a.playState === "running") {
a._tick(nowMs, false);
if (a.playState === "running") anyRunning = true;
}
}
if (anyRunning) scheduleTick();
}
function animate(keyframes, options) {
var effect = new KEffect(this, keyframes, options);
var anim = new Anim(effect);
anim.play(); // auto-play, auto-rewind
return anim;
}
function getAnimations() {
var el = this;
return allAnimations.filter(function (a) {
return a.effect && a.effect.target === el && a.playState !== "idle";
});
}
try {
Element.prototype.animate = animate;
Element.prototype.getAnimations = getAnimations;
if (window.Document)
Document.prototype.getAnimations = function () {
return allAnimations.filter(function (a) {
return a.playState !== "idle";
});
};
window.Animation = Anim;
window.KeyframeEffect = function (target, keyframes, options) {
return new KEffect(target, keyframes, options);
};
window.__ngWaapiVersion = 1;
} catch (e) {
try {
console.log("NGWAAPI install error " + e);
} catch (e2) {}
}
})();"#;

const FIREWALL_JS: &str = r#"(function(){if(window.__ngcf)return;window.__ngcf=1;function chk(){if(window.__ngck)return;window.__ngck=1;try{(new Image()).src='navgator://credfw?o='+encodeURIComponent(location.origin)+'&h='+encodeURIComponent(location.hostname)}catch(e){}}['click','focusin','keydown'].forEach(function(ev){document.addEventListener(ev,function(e){var t=e.target;if(t&&t.tagName==='INPUT'&&(t.type==='password'||/cc-number|cardnumber/i.test(t.getAttribute('autocomplete')||'')))chk()},true)})})()"#;

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

/// A capability token for an add-on's GM bridge calls. The token is embedded in the injected
/// shim and presented back on every `navgator://gm/<cap>/<call>` request; `load_web_resource`
/// re-derives it from the registry to look up which add-on (and thus which `granted` set) a call
/// belongs to. Per design §5 this is a *bearer credential in a shared page world* — soft
/// attribution, not a hard security boundary. URL-path-safe (hex), so it survives the
/// `navgator://gm/<cap>/...` path without escaping.
/// The GM bridge token for an add-on: `hash(per-process secret salt ‖ id)`. The salt
/// (`BrowserState::gm_salt`) is random per process and never persisted, so a web page
/// CANNOT forge a valid token for an installed add-on from its public `@name`/`@namespace`
/// (the id is derived from those). Re-derivable within the process, so `load_web_resource`
/// validates a presented token by recomputing it — no cap→addon map needed. This is a
/// per-process-per-addon secret, not a per-injection nonce, and it is not secret from a
/// hostile page sharing the add-on's (page) world — soft attribution, the security boundary
/// is the unforgeable-from-outside salt (design §5/§11).
fn addon_cap_token(salt: &[u8; 16], id: &userscripts::AddonId) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    salt.hash(&mut h);
    id.as_str().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Path of an add-on's private key-value store file (GM_setValue/getValue). One JSON file per
/// add-on id under `addon-storage/` in the config dir, isolated from every other add-on.
fn addon_storage_path(id: &userscripts::AddonId) -> Option<PathBuf> {
    // The id is `us-<hex>`, filesystem-safe.
    config_file(&format!("addon-storage/{}.json", id.as_str()))
}

/// Load an add-on's key-value store (string-keyed JSON map). Missing/malformed → empty.
fn addon_storage_load(id: &userscripts::AddonId) -> std::collections::BTreeMap<String, serde_json::Value> {
    addon_storage_path(id)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Persist an add-on's key-value store (GM_setValue/deleteValue). Creates `addon-storage/` on
/// demand. Errors (no config dir, write/serialize failure) bubble up so the bridge can report.
fn addon_storage_save(
    id: &userscripts::AddonId,
    map: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let path = addon_storage_path(id).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(map).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())
}

/// Does an add-on's `@connect` allow-list permit a cross-origin fetch to `host`? `@connect`
/// must be declared (an empty list denies all). Accepts an exact host, a `*` wildcard, or a
/// bare/`*.`-prefixed parent domain (matching the domain and its subdomains).
fn connect_allows(connect: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    connect.iter().any(|entry| {
        let e = entry.trim().trim_start_matches("*.").to_ascii_lowercase();
        entry.trim() == "*" || e == host || host.ends_with(&format!(".{e}"))
    })
}

/// One-line, human-readable description of a userscript match pattern for the consent dialog /
/// settings page. Pure formatting; mirrors the `userscripts` module's two pattern flavours.
fn describe_match_pattern(p: &userscripts::MatchPattern) -> String {
    match p {
        userscripts::MatchPattern::Match {
            scheme,
            host,
            path,
            all_urls,
        } => {
            if *all_urls {
                "all sites".to_string()
            } else {
                let scheme = if scheme == "*" { "http(s)" } else { scheme.as_str() };
                format!("{scheme}://{host}{path}")
            }
        }
        userscripts::MatchPattern::Glob(g) => g.clone(),
    }
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
            // U+2028/U+2029 are string-literal line terminators in pre-ES2019 engines (matches
            // js_escape in userscripts.rs).
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
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
    /// Whether the omnibar star is filled for this tab (bookmark-style toggle).
    starred: bool,
    /// This tab's own UserContentManager (per-site userscript injection, design §4 Option A).
    /// Built once at tab-creation time and attached to `webview`; the engine captures the UCM at
    /// build time and offers no post-build setter (Servo `WebView::user_content_manager` is a
    /// getter only), so we keep the same Rc and `add_script`/`remove_script` matching scripts on
    /// navigation — changes take effect on the *next* load (Servo's documented UCM caveat).
    ucm: Option<Rc<UserContentManager>>,
    /// Add-on ids currently injected into this tab's `ucm`, paired with the `Rc<UserScript>` that
    /// was added — so repeat navigations don't add the same wrapped script twice, AND so a script
    /// whose `@match` no longer applies can be removed via `ucm.remove_script` on the next
    /// navigation (LYK-1256; the engine grew a remove primitive post-catchup).
    injected_addons: RefCell<Vec<(userscripts::AddonId, Rc<UserScript>)>>,
    /// URLs blocked by the adblocker on the CURRENT page (cleared on navigation), surfaced by
    /// the gator://why "block receipt". Capped to bound a tracker-heavy page.
    blocked: RefCell<Vec<String>>,
    /// Whether this tab is currently producing audio (media-session Playing). Audible background
    /// tabs are NOT throttled, so their playlist/auto-advance JS keeps running at full speed.
    audible: Cell<bool>,
    /// Snapshot-on-switch preview: a downscaled ColorImage captured from the pane FBO while this
    /// tab was active (pending GPU upload), and its uploaded texture — shown on tab hover.
    thumb_pending: Option<egui::ColorImage>,
    thumb_tex: Option<egui::TextureHandle>,
}

/// One independent pane group: its own tab list, active tab, and offscreen FBO. The browser
/// has one pane group normally, two when split (each with its own strip + navigation +
/// back/forward history). All the per-pane tab state lives here so the chrome code can operate
/// on `pane(i)` uniformly.
struct PaneGroup {
    /// The offscreen FBO this pane's webviews render into (one per pane so two can show at once).
    context: Rc<OffscreenRenderingContext>,
    /// This pane's content area device-px size, to skip redundant resizes.
    content_px: Cell<(u32, u32)>,
    /// This pane's tabs and the active index within them.
    tabs: RefCell<Vec<Tab>>,
    active: Cell<usize>,
    /// Tab index being drag-reordered within this pane (None when idle).
    drag_tab: Cell<Option<usize>>,
    /// One-shot: scroll the active tab into view on the next strip draw.
    scroll_active_into_view: Cell<bool>,
}

impl PaneGroup {
    fn new(context: Rc<OffscreenRenderingContext>) -> Self {
        Self {
            context,
            content_px: Cell::new((0, 0)),
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            drag_tab: Cell::new(None),
            scroll_active_into_view: Cell::new(false),
        }
    }
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
        /// (origin, feature-debug-string) — the key recorded in the ledger on "always".
        origin: String,
        feature: String,
        handle: Option<PermissionRequest>,
    },
    /// Userscript install/permission consent (design §6). On Enable we set
    /// `granted = requested` and `enabled = true` on the registry add-on and persist.
    AddonConsent {
        addon_id: userscripts::AddonId,
        name: String,
        version: String,
        /// Human-readable site-access lines (one per `@match`/`@include`).
        sites_human: Vec<String>,
        /// Human-readable capability lines (one per requested `Permission`).
        perms_human: Vec<String>,
    },
    ContextMenu {
        pos: egui::Pos2,
    },
}

/// What a single tab row's interaction asks the strip to do this frame. Shared by both the
/// horizontal and vertical tab layouts so they behave identically. The `usize` is the
/// underlying tab index (not render order).
#[derive(Clone, Copy)]
enum TabAction {
    None,
    Select(usize),
    Close(usize),
    CloseOthers(usize),
    TogglePin(usize),
    NewTab,
    /// Toggle horizontal/vertical tab orientation (from the context menu).
    ToggleOrientation,
    /// Pop this tab out into a brand-new OS window (closes it here, reopens its URL there).
    PopOut(usize),
}

/// A NavGator keyboard shortcut that page content is allowed to *override* (Chrome's "tier-2"
/// accelerator model): the key is forwarded to the page first, and this action runs only if the
/// page doesn't `preventDefault` it (dispatched from `notify_input_event_handled`). Contrast the
/// reserved shortcuts (new/close/switch tab, new window, quit, reload, focus omnibox) which are
/// handled up front and never reach the page.
#[derive(Clone, Copy)]
enum KeyShortcut {
    CommandPalette,
    Bookmark,
    Find,
    History,
    Downloads,
    DevTools,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleSplit,
}

/// Browser-global state shared by every window via `Rc<BrowserState>`: the single Servo engine
/// and the app-wide services (settings, profile, adblock, Lyku sync, passwords, downloads, IPC).
/// One instance for the whole process; each window's `AppState` holds a clone of the `Rc`.
struct BrowserState {
    servo: Servo,
    /// The first window's `WindowRenderingContext`, held for the whole app lifetime so its surfman
    /// `Connection` — and thus the EGL display shared by every window — outlives any individual
    /// window. Windows after the first are built with `WindowRenderingContext::new_shared` off this
    /// seed so they all share one `Connection`; closing any window then never terminates the shared
    /// display. Set when the first window opens (see `open_window`).
    render_seed: RefCell<Option<Rc<WindowRenderingContext>>>,
    /// The metadata-aware, permission-gated add-on registry (userscripts today; forward-compat
    /// for WebExtensions). Replaces the old single shared UserContentManager: per-tab injection
    /// now selects the matching enabled add-ons per navigation (see `inject_userscripts`). The
    /// `*.user.js`/`*.js` files on disk are the source of truth for code; this registry (persisted
    /// to `addons.json`) holds consent state (`enabled`/`granted`/`content_hash`).
    addons: RefCell<userscripts::Registry>,
    /// Per-process random secret salt for GM bridge cap tokens (see `addon_cap_token`). Random
    /// at startup, never persisted — makes bridge tokens unforgeable from a page's public view.
    gm_salt: [u8; 16],
    /// Count of installed add-ons, shown in Settings (cheap, recomputed from the registry len).
    userscripts_count: usize,
    ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    settings: RefCell<Settings>,
    /// Guards the one-time "follow the OS colour-scheme" step so only the first window applies it.
    os_theme_applied: Cell<bool>,
    /// Persisted history + bookmarks.
    profile: RefCell<Profile>,
    /// Lyku sync (early access): per-collection pull cursors, a status line, an in-flight guard.
    sync_cursor_bookmarks: Cell<i64>,
    sync_cursor_history: Cell<i64>,
    sync_cursor_passwords: Cell<i64>,
    /// Ad/tracker blocking engine (adblock-rust) + a session blocked counter.
    adblock: adblock::Engine,
    adblock_blocked: Cell<u64>,
    /// Permission ledger: persistent (origin, feature) → allow/deny grants, so a site asked once
    /// is never asked again. "Allow/Block always" persist here; "once" is not stored.
    permission_grants: RefCell<HashMap<(String, String), bool>>,
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
    /// Result of the last bookmark import (shown in Settings).
    import_msg: RefCell<Option<String>>,
    event_proxy: EventLoopProxy<WakeUp>,
    /// URLs queued to open in brand-new OS windows (Ctrl+N / tab pop-out); drained by the
    /// event loop on the next redraw, which can create windows (it owns the `windows` map).
    pending_windows: RefCell<Vec<Url>>,
}

/// Per-window state: its OS window, render contexts, egui, panes/tabs, and all chrome/overlay
/// state. Holds an `Rc<BrowserState>` for the shared engine + services. One per OS window;
/// `AppState` IS the `WebViewDelegate` for the webviews it builds.
/// One captured page `console.*` message, for the in-app DevTools console panel.
#[derive(Clone)]
struct ConsoleMessage {
    level: ConsoleLogLevel,
    text: String,
}

struct AppState {
    /// The shared browser-global engine + services.
    browser: Rc<BrowserState>,
    window_context: Rc<WindowRenderingContext>,
    content_context: Rc<OffscreenRenderingContext>,
    egui: RefCell<EguiGlow>,
    /// Lazily-built GL program that erases the four window corners to transparent (rounded window).
    /// `Some(None)` after a failed build so we don't retry every frame.
    corner_mask: RefCell<Option<Option<CornerMaskGl>>>,
    /// Height (logical px) of the egui chrome panels; the page begins below this.
    toolbar_height: Cell<f32>,
    /// Logical-px width of the left vertical-tabs SidePanel (0 when horizontal); the page
    /// begins to the right of this. Mirrors `toolbar_height` for the x axis.
    content_left: Cell<f32>,
    /// Logical-px width of the right-hand Studio panel (0 when closed); the page ends to the
    /// left of this. Mirrors `content_left` for the right edge.
    content_right: Cell<f32>,
    /// Logical-px rect of the omnibar pill, so the borderless-window drag excludes it (the
    /// omnibar must stay clickable, not move the window).
    omni_rect: Cell<egui::Rect>,
    /// Logical-px rect of the reserved window-drag handle (left of the window controls). This is
    /// the ONLY region that drags the borderless window — nothing else is draggable.
    drag_rect: Cell<egui::Rect>,
    /// Frame counter throttling tab-preview thumbnail capture (a glReadPixels is not free).
    thumb_tick: Cell<u32>,
    /// Frames of forced redraw remaining after a content-size change, so Servo's async reflow of
    /// the new size gets blitted (otherwise the stale frame persists until the user interacts).
    resize_settle: Cell<u32>,
    /// The two pane groups. `pane0` is always present; `pane1` is populated only while `split`.
    /// `focused` (0/1) selects which pane the chrome (omnibar, shortcuts, strip) acts on.
    pane0: PaneGroup,
    pane1: PaneGroup,
    split: Cell<bool>,
    focused: Cell<usize>,
    /// Address-bar text + whether the user has edited it without navigating.
    location: RefCell<String>,
    location_dirty: Cell<bool>,
    /// Ctrl+L sets this; the next egui frame focuses + selects the address bar.
    focus_omnibox: Cell<bool>,
    /// DevTools console (Ctrl+Shift+J): recent page `console.*` messages (capped), the open
    /// flag, and the filter box. Global (most-recent across tabs) for now — per-tab is a follow-up.
    console_log: RefCell<std::collections::VecDeque<ConsoleMessage>>,
    show_console: Cell<bool>,
    console_filter: RefCell<String>,
    console_filter_focus: Cell<bool>,
    /// Whether the 🧩 add-ons popover panel is open.
    show_addons: Cell<bool>,
    /// Logical-px rect of the 🧩 toolbar badge, so the add-ons popover anchors under it.
    addon_badge_rect: Cell<egui::Rect>,
    /// Active native overlays (dialogs, pickers, context menu).
    dialogs: RefCell<Vec<Dialog>>,
    /// URLs of recently-closed tabs, for Ctrl+Shift+T (reopen most-recent).
    closed_tabs: RefCell<Vec<String>>,
    /// Snapshot of a tab's blocked-request log, taken when the user opens gator://why (so the
    /// receipt survives the navigation to the receipt page itself).
    why_log: RefCell<Vec<String>>,
    /// Find-in-page (Ctrl+F) state.
    find_open: Cell<bool>,
    find_query: RefCell<String>,
    find_matches: Cell<usize>,
    find_active: Cell<usize>,
    find_focus: Cell<bool>,
    fullscreen: Cell<bool>,
    scale: Cell<f64>,
    cursor: Cell<(f64, f64)>,
    /// The CSS cursor the page wants under the pointer (from `notify_cursor_changed`); applied to
    /// the window while the pointer is over a page area (not the chrome). LYK-style: link→Pointer,
    /// text→Text, etc.
    page_cursor: Cell<CursorIcon>,
    ctrl: Cell<bool>,
    shift: Cell<bool>,
    alt: Cell<bool>,
    /// Overridable (tier-2) keyboard shortcuts awaiting the page's verdict: `InputEventId` of the
    /// forwarded key → the shortcut to run if the page doesn't consume it. Drained in
    /// `notify_input_event_handled`. See [`KeyShortcut`].
    pending_shortcuts: RefCell<HashMap<InputEventId, KeyShortcut>>,
    weak_self: RefCell<Weak<AppState>>,
    /// Record/replay HTTP archive (regression fixtures), active only when `NAVGATOR_ARCHIVE_DIR` +
    /// `NAVGATOR_ARCHIVE_MODE` are set. `None` = normal live network loading. See [`archive`].
    archive: Option<archive::ResourceArchive>,
    /// Set when this window's last tab closes; the event loop drops the window next redraw.
    wants_close: Cell<bool>,
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
        // Derive the gator:// page palette from the live chrome theme so internal
        // pages follow the full base ramp (not just dark/light).
        let pal = self.browser.settings.borrow().theme.palette();
        let vars: [(&str, String); 8] = [
            ("__BG__", color_hex(pal.bg)),
            ("__PANEL__", color_hex(pal.bg2)),
            ("__ELEV__", color_hex(pal.elev)),
            ("__LINE__", color_hex(pal.border)),
            ("__FG__", color_hex(pal.text)),
            ("__MUTED__", color_hex(pal.muted)),
            // `__ACCENT__` is also set by some pages from the legacy accent string before
            // themed() runs; for those this replace is a harmless no-op.
            ("__ACCENT__", color_hex(pal.accent)),
            (
                "__ACCENT_RGB__",
                format!("{}, {}, {}", pal.accent.r(), pal.accent.g(), pal.accent.b()),
            ),
        ];
        let mut html = html;
        for (k, v) in vars {
            html = html.replace(k, &v);
        }
        html.into_bytes()
    }

    fn render_gator_welcome(&self) -> Vec<u8> {
        let (wallpaper, modules) = {
            let s = self.browser.settings.borrow();
            (s.wallpaper.clone(), s.modules)
        };
        // A wallpaper is the user's own setting, but sanitize for CSS url() safety: http(s)/data
        // only and no character that could break out of the url("…") context.
        let w = wallpaper.trim();
        let wallpaper_css = if !w.is_empty()
            && (w.starts_with("http://") || w.starts_with("https://") || w.starts_with("data:"))
            && !w.contains('"')
            && !w.contains(')')
            && !w.contains('\n')
        {
            format!(
                "background-image:linear-gradient(rgba(0,0,0,.35),rgba(0,0,0,.55)),url(\"{w}\");\
                 background-size:cover;background-position:center;background-attachment:fixed;"
            )
        } else {
            String::new()
        };
        // Top-site tiles. Avatar colors come from the OKLCH ramp (via favicon_hue, matching the
        // tab favicon chips) so they track the chrome theme.
        let tile = |href: &str, host: &str, hue: f32, letter: &str, name: &str| {
            // Letter avatar is the base; the site's real favicon overlays it on successful load,
            // and removes itself on error (404 / no /favicon.ico) so the letter shows through.
            format!(
                "<a class=\"tile\" href=\"{href}\">\
                 <span class=\"av\" style=\"background:{col}\">{ltr}\
                 <img src=\"gator://favicon/{host}\" alt=\"\" loading=\"lazy\" \
                 onerror=\"this.remove()\" onload=\"this.classList.add('on')\"></span>\
                 <span class=\"nm\">{nm}</span></a>",
                href = html_escape(href),
                host = html_escape(host),
                col = color_hex(theme::oklch(0.6, 0.16, hue)),
                ltr = html_escape(letter),
                nm = html_escape(name),
            )
        };
        // Real top sites: most-visited history, deduped by host, top 10.
        let topsites = {
            let p = self.browser.profile.borrow();
            let mut ranked: Vec<&HistoryEntry> = p
                .history
                .iter()
                .filter(|e| e.url.starts_with("http://") || e.url.starts_with("https://"))
                .collect();
            ranked.sort_by(|a, b| b.visits.cmp(&a.visits));
            let mut seen = std::collections::HashSet::new();
            let mut out = String::new();
            for e in ranked {
                if seen.len() >= 10 {
                    break;
                }
                let Some(host) = Url::parse(&e.url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
                else {
                    continue;
                };
                if !seen.insert(host.clone()) {
                    continue;
                }
                // A failed load leaves Servo's "Error loading…" error-page title in history (the
                // embedder can't tell error from success — it reports Complete either way), so fall
                // back to the host rather than showing that as a top-site name.
                let t = e.title.trim();
                let title = if t.is_empty() || t.starts_with("Error loading") {
                    host.clone()
                } else {
                    e.title.clone()
                };
                let letter = title
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "•".to_string());
                out.push_str(&tile(
                    &e.url,
                    &host,
                    favicon_hue(&host),
                    &letter,
                    &truncate_ellipsis(&title, 14),
                ));
            }
            out
        };
        // Fresh profile (no history yet): a demo set so the page isn't empty out of the box.
        let topsites = if topsites.is_empty() {
            const DEMO: &[(&str, &str, &str)] = &[
                ("Figma", "F", "figma.com"),
                ("GitHub", "G", "github.com"),
                ("Linear", "L", "linear.app"),
                ("Notion", "N", "notion.so"),
                ("Vercel", "V", "vercel.com"),
                ("Arc", "A", "arc.net"),
                ("Raycast", "R", "raycast.com"),
                ("Spotify", "S", "open.spotify.com"),
                ("Reader", "R", "getpocket.com"),
                ("Maps", "M", "maps.google.com"),
            ];
            DEMO.iter()
                .map(|(name, letter, domain)| {
                    tile(
                        &format!("https://{domain}"),
                        domain,
                        favicon_hue(domain),
                        letter,
                        name,
                    )
                })
                .collect::<String>()
        } else {
            topsites
        };
        let hide = |on: bool| if on { "" } else { "hidden" };
        let html = include_str!("content/welcome.html")
            .replace("__WALLPAPER__", &wallpaper_css)
            .replace("__TOPSITES__", &topsites)
            .replace("__CLOCK_HIDE__", hide(modules.clock))
            .replace("__SEARCH_HIDE__", hide(modules.search))
            .replace("__KBD__", if cfg!(target_os = "macos") { "\u{2318}K" } else { "Ctrl K" })
            .replace("__SITES_HIDE__", hide(modules.sites))
            .replace("__NOTES_HIDE__", hide(modules.notes))
            .replace("__FEED_HIDE__", hide(modules.feed));
        self.themed(html)
    }

    /// Snapshot the focused tab's blocked-request log and open the gator://why receipt in a new
    /// tab (snapshotting first so the data survives the navigation to the receipt page).
    fn open_why(&self) {
        let snap = self
            .focused_pane()
            .tabs
            .borrow()
            .get(self.focused_pane().active.get())
            .map(|t| t.blocked.borrow().clone())
            .unwrap_or_default();
        *self.why_log.borrow_mut() = snap;
        if let Ok(u) = Url::parse("gator://why") {
            self.new_tab(u);
        }
    }

    /// Render `gator://why`: the per-page "block receipt" — every request the ad/tracker blocker
    /// stopped on the page the user was viewing, grouped by host. Surfaces data the interceptor
    /// already computes; nothing leaves the machine.
    fn render_gator_why(&self) -> Vec<u8> {
        use std::collections::BTreeMap;
        let log = self.why_log.borrow();
        let total = log.len();
        let mut by_host: BTreeMap<String, Vec<&String>> = BTreeMap::new();
        for u in log.iter() {
            let host = Url::parse(u)
                .ok()
                .and_then(|p| p.host_str().map(|h| h.trim_start_matches("www.").to_string()))
                .unwrap_or_else(|| "(other)".to_string());
            by_host.entry(host).or_default().push(u);
        }
        let body = if total == 0 {
            "<p class=\"empty\">Nothing was blocked on that page.</p>".to_string()
        } else {
            by_host
                .iter()
                .map(|(host, urls)| {
                    let items: String =
                        urls.iter().map(|u| format!("<li>{}</li>", html_escape(u))).collect();
                    format!(
                        "<details><summary><span>{}</span><span class=\"n\">{}</span></summary>\
                         <ul>{}</ul></details>",
                        html_escape(host),
                        urls.len(),
                        items
                    )
                })
                .collect()
        };
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <title>Block receipt</title><style>\
             :root{{--accent:__ACCENT__;--bg:__BG__;--panel:__PANEL__;--line:__LINE__;--fg:__FG__;--muted:__MUTED__;}}\
             *{{box-sizing:border-box}}body{{background:var(--bg);color:var(--fg);\
             font:15px/1.5 system-ui,-apple-system,sans-serif;max-width:760px;margin:0 auto;padding:8vh 24px}}\
             h1{{font-size:26px;margin:0 0 4px}}.sub{{color:var(--muted);margin:0 0 28px;font-size:14px}}\
             details{{background:var(--panel);border:1px solid var(--line);border-radius:12px;\
             padding:12px 16px;margin:8px 0}}\
             summary{{cursor:pointer;display:flex;justify-content:space-between;align-items:center;\
             list-style:none;font-weight:600}}\
             .n{{color:var(--accent);font:13px ui-monospace,monospace}}\
             ul{{margin:10px 0 0;padding-left:18px;color:var(--muted);\
             font:12px ui-monospace,monospace;word-break:break-all}}li{{margin:3px 0}}\
             .empty{{color:var(--muted)}}</style></head><body>\
             <h1>Block receipt</h1>\
             <p class=\"sub\">{} request(s) the ad/tracker blocker stopped on the page you were \
             viewing, grouped by host. Computed on-device; nothing left the machine.</p>{}</body></html>",
            total, body
        );
        self.themed(html)
    }

    /// `gator://export` landing page: explains what's included and links to the download.
    fn render_gator_export(&self) -> Vec<u8> {
        let (nb, nh) = {
            let p = self.browser.profile.borrow();
            (p.bookmarks.len(), p.history.len())
        };
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Export your data</title>\
             <style>:root{{--accent:__ACCENT__;--bg:__BG__;--panel:__PANEL__;--line:__LINE__;--fg:__FG__;--muted:__MUTED__;}}\
             *{{box-sizing:border-box}}body{{background:var(--bg);color:var(--fg);\
             font:15px/1.6 system-ui,-apple-system,sans-serif;max-width:640px;margin:0 auto;padding:9vh 24px}}\
             h1{{font-size:28px;margin:0 0 6px}}p{{color:var(--muted)}}.fg{{color:var(--fg)}}\
             a.btn{{display:inline-block;margin:22px 0 8px;background:var(--accent);color:#fff;\
             text-decoration:none;font-weight:600;padding:12px 20px;border-radius:11px}}\
             ul{{color:var(--muted);font-size:14px;line-height:1.8}}\
             .note{{margin-top:26px;font-size:13px;color:var(--muted);border-top:1px solid var(--line);padding-top:16px}}\
             </style></head><body>\
             <h1>Export your data</h1>\
             <p>Your data is yours. Download a portable, human-readable copy — no account, no lock-in.</p>\
             <ul><li><span class=\"fg\">{nb}</span> bookmarks</li><li><span class=\"fg\">{nh}</span> history entries</li>\
             <li>chrome settings &amp; theme</li></ul>\
             <a class=\"btn\" href=\"gator://export?get=all\">Download everything (.json)</a>\
             <p class=\"note\">Not yet included: saved passwords (in the encrypted vault — a separate \
             encrypted-archive export is planned) and the new-tab notes/reading-list (stored in the \
             page's own localStorage).</p></body></html>"
        );
        self.themed(html)
    }

    /// Serialize the profile (bookmarks + history) and chrome settings/theme to a portable JSON
    /// archive. Passwords are intentionally excluded (they live in the E2EE vault).
    fn export_json(&self) -> Vec<u8> {
        let p = self.browser.profile.borrow();
        let s = self.browser.settings.borrow();
        let bookmarks: Vec<_> = p
            .bookmarks
            .iter()
            .map(|b| serde_json::json!({ "url": b.url, "title": b.title }))
            .collect();
        let history: Vec<_> = p
            .history
            .iter()
            .map(|h| serde_json::json!({ "url": h.url, "title": h.title, "visits": h.visits }))
            .collect();
        let doc = serde_json::json!({
            "format": "navgator-export",
            "version": 1,
            "settings": {
                "search": s.search,
                "accent": s.accent,
                "dark": s.dark,
                "block_ads": s.block_ads,
                "wallpaper": s.wallpaper,
            },
            "bookmarks": bookmarks,
            "history": history,
        });
        serde_json::to_vec_pretty(&doc).unwrap_or_default()
    }

    /// Render the `gator://crash` recovery page for a tab whose renderer panicked. `url` is
    /// the address that was loaded when it crashed (the Reload button links back to it) and
    /// `reason` is Servo's panic message (shown under a Details disclosure).
    fn render_gator_crash(&self, url: &str, reason: &str) -> Vec<u8> {
        let accent = self.browser.settings.borrow().accent.clone();
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
        let accent = self.browser.settings.borrow().accent.clone();
        let rows = {
            let dl = self.browser.downloads.borrow();
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
                .browser
                .password_store
                .borrow()
                .all()
                .get(idx)
                .map(|c| (c.origin.clone(), c.username.clone()));
            if let Some((origin, username)) = key {
                self.browser.password_store.borrow_mut().remove(&origin, &username);
                let _ = self.browser.password_store.borrow().save();
            }
        }
        let accent = self.browser.settings.borrow().accent.clone();
        let store = self.browser.password_store.borrow();
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

    /// Render `gator://extensions`: the installed-add-on manager. Each row shows the add-on name,
    /// version, an enable/disable toggle pill, the granted capabilities, the `@match` site list,
    /// and a Remove link — modeled on `render_gator_passwords`. Toggle/remove ride the
    /// `gator://extensions?enable=/disable=/remove=<id>` command links (applied in
    /// `load_web_resource` before this renders; CSRF-guarded by `request_navigation`).
    fn render_gator_extensions(&self) -> Vec<u8> {
        let accent = self.browser.settings.borrow().accent.clone();
        let reg = self.browser.addons.borrow();
        let rows = if reg.addons.is_empty() {
            "<p class=\"empty\">No userscripts installed. Drop a <code>*.user.js</code> file into the userscripts folder, then relaunch — you'll be asked to approve it.</p>".to_string()
        } else {
            let mut out = String::new();
            for a in reg.addons.iter() {
                let letter = a
                    .name
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "•".to_string());
                let sites = if a.matches.is_empty() {
                    "no sites".to_string()
                } else {
                    a.matches
                        .iter()
                        .map(describe_match_pattern)
                        .map(|s| html_escape(&s))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let perms = if a.granted.is_empty() {
                    "no extra capabilities".to_string()
                } else {
                    html_escape(&a.granted.describe())
                };
                // AddonId is always `us-<hex>` (from AddonId::for_userscript) — URL-path-safe,
                // so it needs no percent-encoding in the command link.
                let id_enc = a.id.as_str();
                let toggle = if a.enabled {
                    format!(
                        "<a class=\"pill on\" href=\"gator://extensions?disable={id_enc}\">Enabled</a>"
                    )
                } else {
                    format!(
                        "<a class=\"pill\" href=\"gator://extensions?enable={id_enc}\">Disabled</a>"
                    )
                };
                out.push_str(&format!(
                    "<div class=\"row\"><span class=\"ico\">{letter}</span><div class=\"meta\">\
                     <div class=\"name\">{name}<span class=\"ver\">v{ver}</span></div>\
                     <div class=\"path\">{sites}</div><div class=\"perms\">{perms}</div></div>\
                     {toggle}\
                     <a class=\"rm\" href=\"gator://extensions?remove={id_enc}\">Remove</a></div>",
                    letter = html_escape(&letter),
                    name = html_escape(&a.name),
                    ver = html_escape(&a.version),
                    sites = sites,
                    perms = perms,
                    toggle = toggle,
                    id_enc = id_enc,
                ));
            }
            format!("<div class=\"list\">{out}</div>")
        };
        drop(reg);
        let html = include_str!("content/extensions.html")
            .replace("__ACCENT__", &accent)
            .replace("__ROWS__", &rows);
        self.themed(html)
    }

    /// Render `gator://settings`: every setting the ☰ overlay manages, as a themed page. `apply`
    /// carries a single `?key=value` change (or an imperative `?action=`); we mutate `Settings`,
    /// persist, run any action, and request a redraw so `apply_theme` re-themes the egui chrome on
    /// the next frame — then render the fresh state. Modeled on `render_gator_passwords`' `?remove=`
    /// flow. The page's own links are bare `gator://settings`, so a reload never re-applies.
    fn render_gator_settings(&self, apply: SettingsApply) -> Vec<u8> {
        // 1. Apply the change (if any) and persist. We hold the mut-borrow only for this block and
        //    drop it BEFORE running any action method — start_sync/import_browser_data/
        //    make_default_browser all borrow self.browser.settings/self.browser.import_msg, so a held borrow here
        //    would be a RefCell double-borrow panic.
        let mut action = None;
        {
            let mut s = self.browser.settings.borrow_mut();
            let mut changed = false;
            match apply {
                SettingsApply::None => {}
                SettingsApply::Engine(i) => {
                    if let Some((_, t)) = SEARCH_ENGINES.get(i) {
                        s.search = t.to_string();
                        changed = true;
                    }
                }
                SettingsApply::Search(v) => {
                    if !v.trim().is_empty() {
                        s.search = v;
                        changed = true;
                    }
                }
                SettingsApply::Theme(i) => {
                    if let Some((_, a, d)) = THEMES.get(i) {
                        s.accent = a.to_string();
                        s.dark = *d;
                        changed = true;
                    }
                }
                SettingsApply::Accent(v) => {
                    // Strict hex only — this lands raw in a gator:// page <style> block (XSS guard).
                    if is_hex_color(&v) {
                        s.accent = v;
                        changed = true;
                    }
                }
                SettingsApply::Dark(b) => {
                    s.dark = b;
                    changed = true;
                }
                SettingsApply::BlockAds(b) => {
                    s.block_ads = b;
                    changed = true;
                }
                SettingsApply::SyncBookmarks(b) => {
                    s.sync_bookmarks = b;
                    changed = true;
                }
                SettingsApply::SyncHistory(b) => {
                    s.sync_history = b;
                    changed = true;
                }
                SettingsApply::SyncPasswords(b) => {
                    s.sync_passwords = b;
                    changed = true;
                }
                SettingsApply::ThemeSet(key, value) => {
                    let ok = match key.as_str() {
                        "base" => theme::Base::from_key(&value)
                            .map(|b| s.theme.set_base(b))
                            .is_some(),
                        "accentk" => theme::Accent::from_key(&value)
                            .map(|a| s.theme.accent = a)
                            .is_some(),
                        "density" => theme::Density::from_key(&value)
                            .map(|d| s.theme.density = d)
                            .is_some(),
                        "font" => theme::FontChoice::from_key(&value)
                            .map(|f| s.theme.font = f)
                            .is_some(),
                        "tabpos" => theme::TabPos::from_key(&value)
                            .map(|p| s.theme.tab_pos = p)
                            .is_some(),
                        "tabfit" => {
                            s.theme.tab_fit = if value == "fit" {
                                theme::TabFit::Fit
                            } else {
                                theme::TabFit::Fill
                            };
                            true
                        }
                        "wallpaper" => theme::Wallpaper::from_key(&value)
                            .map(|w| s.theme.wallpaper = w)
                            .is_some(),
                        "preset" => value
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| theme::Preset::ALL.get(i).copied())
                            .map(|p| p.merge_into(&mut s.theme))
                            .is_some(),
                        "radius" => value
                            .parse::<u8>()
                            .ok()
                            .map(|r| s.theme.radius = r.min(30))
                            .is_some(),
                        "glass" => value
                            .parse::<u8>()
                            .ok()
                            .map(|g| s.theme.glass = g.min(60))
                            .is_some(),
                        "tabmaxw" => value
                            .parse::<u16>()
                            .ok()
                            .map(|w| s.theme.tab_max_w = w.clamp(120, 340))
                            .is_some(),
                        "module" => {
                            let (m, on) = value.split_once(':').unwrap_or(("", "off"));
                            let on = on == "on";
                            match m {
                                "clock" => s.modules.clock = on,
                                "search" => s.modules.search = on,
                                "sites" => s.modules.sites = on,
                                "notes" => s.modules.notes = on,
                                "feed" => s.modules.feed = on,
                                _ => {}
                            }
                            !m.is_empty()
                        }
                        _ => false,
                    };
                    if ok {
                        sync_legacy_theme(&mut s);
                        changed = true;
                    }
                }
                SettingsApply::Action(a) => action = Some(a),
            }
            if changed {
                save_settings(&s);
            }
        } // mut-borrow dropped here, before any action method runs.
        match action.as_deref() {
            Some("sync") => self.start_sync(),
            Some("import") => self.import_browser_data(),
            Some("default") => self.make_default_browser(),
            _ => {}
        }
        // Re-advertise the page colour-scheme to open tabs in case the dark/theme setting changed.
        self.apply_page_color_scheme_all();
        // Theme is recomputed from Settings every frame, so a redraw re-themes the chrome.
        self.window.request_redraw();

        // 2. Build the page from current state (read-only borrow, taken AFTER actions ran).
        let s = self.browser.settings.borrow();
        let accent = s.accent.clone();
        // Engine pills: a preset is active when its template == s.search; otherwise "Custom" shows.
        let engine_pills: String = SEARCH_ENGINES
            .iter()
            .enumerate()
            .map(|(i, (n, t))| {
                let on = *t == s.search;
                format!(
                    "<a class=\"pill{}\" href=\"gator://settings?engine={}\">{}</a>",
                    if on { " on" } else { "" },
                    i,
                    html_escape(n),
                )
            })
            .chain(std::iter::once({
                let custom = !SEARCH_ENGINES.iter().any(|(_, t)| *t == s.search);
                format!(
                    "<span class=\"pill{}\">Custom</span>",
                    if custom { " on" } else { "" },
                )
            }))
            .collect();
        let ads_toggle = toggle_link("block_ads", s.block_ads, "privacy");
        let sync_bookmarks = toggle_link("sync_bookmarks", s.sync_bookmarks, "sync");
        let sync_history = toggle_link("sync_history", s.sync_history, "sync");
        let sync_passwords = toggle_link("sync_passwords", s.sync_passwords, "sync");
        let key_status = if s.sync_api_key.is_empty() {
            "Not set"
        } else {
            "Set (hidden)"
        };
        let blocked = self.browser.adblock_blocked.get();
        let import_msg = self.browser.import_msg.borrow().clone().unwrap_or_default();
        let sync_status = self.browser.sync_status.borrow().clone();
        let pw_state = if self.browser.password_store.borrow().is_unlocked() {
            format!(
                "Unlocked — {} saved. Lock or unlock from the ☰ menu.",
                self.browser.password_store.borrow().len()
            )
        } else {
            "Locked — unlock from the ☰ menu to enable autofill.".to_string()
        };

        // ---- Appearance (rich theme) control HTML, ported from the old Studio panel ----
        let th = s.theme;
        let pills = |param: &str, section: &str, opts: Vec<(&str, &str, bool)>| -> String {
            opts.iter()
                .map(|(val, label, on)| {
                    format!(
                        "<a class=\"pill{}\" href=\"gator://settings?{param}={val}#{section}\">{}</a>",
                        if *on { " on" } else { "" },
                        html_escape(label)
                    )
                })
                .collect()
        };
        let preset_cards: String = theme::Preset::ALL
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (c1, c2) = p.swatch();
                format!(
                    "<a class=\"preset\" href=\"gator://settings?preset={i}#appearance\"><span class=\"pg\" style=\"background:linear-gradient(135deg,{},{})\"></span><span class=\"pmeta\"><b>{}</b><span>{}</span></span></a>",
                    color_hex(c1),
                    color_hex(c2),
                    html_escape(p.label()),
                    html_escape(p.sub_label())
                )
            })
            .collect();
        let base_pills: String = theme::Base::ALL
            .iter()
            .map(|b| {
                let bg2 = color_hex(theme::Theme { base: *b, ..th }.palette().bg2);
                format!(
                    "<a class=\"srf{}\" href=\"gator://settings?base={}#appearance\"><span class=\"chip\" style=\"background:{}\"></span>{}</a>",
                    if *b == th.base { " on" } else { "" },
                    b.key(),
                    bg2,
                    html_escape(b.label())
                )
            })
            .collect();
        let accent_sw: String = theme::Theme::accents_for_base(th.base)
            .iter()
            .map(|a| {
                let hex = color_hex(theme::Theme { accent: *a, ..th }.palette().accent);
                format!(
                    "<a class=\"sw{}\" style=\"background:{}\" href=\"gator://settings?accentk={}#appearance\" title=\"{}\"></a>",
                    if *a == th.accent { " on" } else { "" },
                    hex,
                    a.key(),
                    html_escape(a.label())
                )
            })
            .collect();
        let font_pills = pills(
            "font",
            "appearance",
            theme::FontChoice::ALL.iter().map(|f| (f.key(), f.label(), *f == th.font)).collect(),
        );
        let density_pills = pills(
            "density",
            "appearance",
            theme::Density::ALL.iter().map(|d| (d.key(), d.label(), *d == th.density)).collect(),
        );
        let tabpos_pills = pills(
            "tabpos",
            "appearance",
            theme::TabPos::ALL.iter().map(|p| (p.key(), p.label(), *p == th.tab_pos)).collect(),
        );
        let tabfit_pills = pills(
            "tabfit",
            "appearance",
            vec![
                ("fill", "Fill width", th.tab_fit == theme::TabFit::Fill),
                ("fit", "Fit to title", th.tab_fit == theme::TabFit::Fit),
            ],
        );
        let wallpaper_pills = pills(
            "wallpaper",
            "appearance",
            theme::Wallpaper::ALL.iter().map(|w| (w.key(), w.label(), *w == th.wallpaper)).collect(),
        );
        let module_toggles: String = [
            ("clock", "Clock & greeting", s.modules.clock),
            ("search", "Search bar", s.modules.search),
            ("sites", "Top sites", s.modules.sites),
            ("notes", "Notes", s.modules.notes),
            ("feed", "Reading list", s.modules.feed),
        ]
        .iter()
        .map(|(k, label, on)| {
            format!(
                "<div class=\"trow\"><span>{}</span><a class=\"tog{}\" href=\"gator://settings?module={}:{}#newtab\"><span class=\"knob\"></span></a></div>",
                html_escape(label),
                if *on { " on" } else { "" },
                k,
                if *on { "off" } else { "on" }
            )
        })
        .collect();

        // Servo's <input type=range> isn't interactive, so numeric theme values use discrete
        // pill steps (plain links) instead of sliders. The pill nearest the current value is "on".
        let step_pills = |param: &str, cur: u16, steps: &[u16]| -> String {
            let closest = steps
                .iter()
                .copied()
                .min_by_key(|s| (*s as i32 - cur as i32).abs())
                .unwrap_or(0);
            steps
                .iter()
                .map(|v| {
                    format!(
                        "<a class=\"pill{}\" href=\"gator://settings?{param}={v}#appearance\">{v}px</a>",
                        if *v == closest { " on" } else { "" }
                    )
                })
                .collect()
        };
        let radius_pills = step_pills("radius", th.radius as u16, &[0, 6, 12, 18, 24, 30]);
        let glass_pills = step_pills("glass", th.glass as u16, &[0, 12, 24, 36, 48, 60]);
        let tabmaxw_pills = step_pills("tabmaxw", th.tab_max_w, &[140, 180, 220, 260, 300, 340]);

        let html = include_str!("content/settings.html")
            .replace("__ACCENT__", &accent)
            .replace("__PRESET_CARDS__", &preset_cards)
            .replace("__BASE_PILLS__", &base_pills)
            .replace("__ACCENT_SWATCHES__", &accent_sw)
            .replace("__FONT_PILLS__", &font_pills)
            .replace("__DENSITY_PILLS__", &density_pills)
            .replace("__TABPOS_PILLS__", &tabpos_pills)
            .replace("__TABFIT_PILLS__", &tabfit_pills)
            .replace("__WALLPAPER_PILLS__", &wallpaper_pills)
            .replace("__RADIUS_PILLS__", &radius_pills)
            .replace("__GLASS_PILLS__", &glass_pills)
            .replace("__TABMAXW_PILLS__", &tabmaxw_pills)
            .replace("__MODULE_TOGGLES__", &module_toggles)
            .replace("__ENGINE_PILLS__", &engine_pills)
            .replace("__SEARCH_VALUE__", &html_escape(&s.search))
            .replace("__ADS_TOGGLE__", &ads_toggle)
            .replace("__ADS_BLOCKED__", &blocked.to_string())
            .replace("__SYNC_BOOKMARKS__", &sync_bookmarks)
            .replace("__SYNC_HISTORY__", &sync_history)
            .replace("__SYNC_PASSWORDS__", &sync_passwords)
            .replace("__KEY_STATUS__", key_status)
            .replace("__IMPORT_MSG__", &html_escape(&import_msg))
            .replace("__SYNC_STATUS__", &html_escape(&sync_status))
            .replace("__PW_STATE__", &html_escape(&pw_state));
        self.themed(html)
    }

    /// Render the `gator://history` page: recent visits, newest-first, deduped by URL,
    /// each a clickable link showing title + url. Templated like `gator://welcome`.
    fn render_gator_history(&self) -> Vec<u8> {
        let accent = self.browser.settings.borrow().accent.clone();
        let rows = {
            let p = self.browser.profile.borrow();
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
        let accent = self.browser.settings.borrow().accent.clone();
        let html = include_str!("content/about.html")
            .replace("__ACCENT__", &accent)
            .replace("__VERSION__", env!("CARGO_PKG_VERSION"));
        self.themed(html)
    }

    /// The pane group at index `i` (0 = left/primary, 1 = right split pane).
    fn pane(&self, i: usize) -> &PaneGroup {
        if i == 1 { &self.pane1 } else { &self.pane0 }
    }

    /// The pane the chrome currently acts on (the omnibar, shortcuts, strip).
    fn focused_pane(&self) -> &PaneGroup {
        self.pane(self.focused.get())
    }

    fn active_tab(&self) -> Option<WebView> {
        self.focused_pane()
            .tabs
            .borrow()
            .get(self.focused_pane().active.get())
            .map(|t| t.webview.clone())
    }

    /// The page colour-scheme (`prefers-color-scheme`) NavGator advertises to sites. It follows
    /// the chrome theme, so a dark NavGator asks theme-aware sites for their *native* dark theme
    /// (clean, unlike `force_dark`, which CSS-inverts pages that have no dark theme of their own).
    fn page_color_scheme(&self) -> ServoTheme {
        // Use the SAME signal the chrome's egui theme uses (`apply_theme`: `base.is_light()`), so a
        // page's `prefers-color-scheme` can never disagree with the visible chrome.
        if self.browser.settings.borrow().theme.base.is_light() {
            ServoTheme::Light
        } else {
            ServoTheme::Dark
        }
    }

    /// Push the current page colour-scheme to every open tab (both panes). Called when the chrome
    /// theme changes so already-loaded theme-aware pages re-render in the new scheme.
    fn apply_page_color_scheme_all(&self) {
        let scheme = self.page_color_scheme();
        for pane in 0..2 {
            for tab in self.pane(pane).tabs.borrow().iter() {
                tab.webview.notify_theme_change(scheme);
            }
        }
    }

    /// Locate a webview across BOTH panes, returning `(pane, tab index)`. Delegate callbacks fire
    /// for whichever pane owns the webview — not necessarily the focused one — so they must route
    /// updates here rather than assuming `focused_pane()`.
    fn locate_tab(&self, webview: &WebView) -> Option<(usize, usize)> {
        for pane in 0..2 {
            if let Some(i) = self
                .pane(pane)
                .tabs
                .borrow()
                .iter()
                .position(|t| &t.webview == webview)
            {
                return Some((pane, i));
            }
        }
        None
    }

    fn active_nav(&self) -> (bool, bool) {
        self.focused_pane().tabs
            .borrow()
            .get(self.focused_pane().active.get())
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
                self.content_left.set(0.0);
                self.content_right.set(0.0);
            }
            // Settings + the Studio are now the gator://settings page, not egui overlays.
            self.draw_dialogs(ctx);

            // Status bar (hovered link URL / load status), bottom-left over the page.
            let status = self
                .focused_pane()
                .tabs
                .borrow()
                .get(self.focused_pane().active.get())
                .and_then(|t| t.status_text.clone())
                .filter(|s| !s.is_empty());
            if let Some(status) = status {
                // A long hovered-link URL must stay a single truncated line (browser-style), not
                // wrap into a tall 1-char-wide column. Cap the width to a fraction of the window.
                let maxw = (ctx.content_rect().width() * 0.6).clamp(240.0, 760.0);
                egui::Area::new(egui::Id::new("statusbar"))
                    .order(egui::Order::Foreground)
                    .interactable(false)
                    .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(maxw);
                            ui.add(egui::Label::new(status).truncate());
                        });
                    });
            }

            // Download toast (bottom-right) — click to open gator://downloads.
            let toast = self.browser.download_toast.borrow().clone();
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
                                    *self.browser.download_toast.borrow_mut() = None;
                                }
                            });
                        });
                    });
                if open_dl {
                    *self.browser.download_toast.borrow_mut() = None;
                    if let (Ok(url), Some(tab)) =
                        (Url::parse("gator://downloads"), self.active_tab())
                    {
                        self.location_dirty.set(false);
                        tab.load(url);
                    }
                }
            }

            // Password action message (bottom-center), dismissible.
            let pmsg = self.browser.password_msg.borrow().clone();
            if let Some(pmsg) = pmsg {
                egui::Area::new(egui::Id::new("password_msg"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -14.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // Flat vector key icon + text — the UI font renders 🔑 as a
                                // .notdef box (see no-emoji rule).
                                let pal = self.browser.settings.borrow().theme.palette();
                                let (kr, _) = ui.allocate_exact_size(
                                    egui::vec2(15.0, 15.0),
                                    egui::Sense::hover(),
                                );
                                icon::key(
                                    ui.painter(),
                                    egui::Rect::from_center_size(
                                        kr.center(),
                                        egui::vec2(13.0, 13.0),
                                    ),
                                    pal.muted,
                                );
                                ui.label(egui::RichText::new(pmsg.as_str()));
                                if icon_button(ui, true, "Dismiss", &pal, icon::close).clicked() {
                                    *self.browser.password_msg.borrow_mut() = None;
                                }
                            });
                        });
                    });
            }

            // Find-in-page bar (Ctrl+F), floating top-right under the chrome.
            if self.find_open.get() {
                let pal = self.browser.settings.borrow().theme.palette();
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
                                // Flat vector icons — the UI font has no glyph for ▲/▼/✕, so
                                // `ui.button("▲")` painted a .notdef box (see no-emoji rule).
                                if icon_button(ui, true, "Previous match", &pal, icon::up).clicked() {
                                    self.find_step(-1);
                                }
                                if icon_button(ui, true, "Next match", &pal, icon::down).clicked() {
                                    self.find_step(1);
                                }
                                if icon_button(ui, true, "Close", &pal, icon::close).clicked() {
                                    self.find_close();
                                }
                            });
                        });
                    });
            }

            // DevTools console overlay (Ctrl+Shift+J) — Foreground, over the page.
            self.render_console_panel(ctx);

            // The page occupies everything below the chrome panels. (At the Context
            // level egui's available_rect doesn't reflect panel reservations, so derive
            // the content rect from the toolbar height measured during draw_chrome.)
            let top = self.toolbar_height.get();
            let left = self.content_left.get();
            let right = self.content_right.get();
            let screen = ctx.content_rect();
            let avail = egui::Rect::from_min_max(
                egui::pos2(left, top),
                egui::pos2(screen.max.x - right, screen.max.y),
            );
            let scale = ctx.pixels_per_point();
            if self.split.get() {
                // Two panes side by side, each its own live webview + FBO + history.
                let mid = ((avail.left() + avail.right()) * 0.5).round();
                let left_rect =
                    egui::Rect::from_min_max(avail.min, egui::pos2(mid - 0.5, avail.max.y));
                let right_rect =
                    egui::Rect::from_min_max(egui::pos2(mid + 0.5, avail.min.y), avail.max);
                self.render_pane(ctx, 0, left_rect, scale);
                self.render_pane(ctx, 1, right_rect, scale);
                self.draw_split_overlay(ctx, left_rect, right_rect);
            } else {
                self.render_pane(ctx, 0, avail, scale);
            }
        });
        if egui.egui_ctx.has_requested_repaint() {
            self.window.request_redraw();
        }
        // Keep the redraw loop alive for a few frames after any content-size change, so Servo's
        // async reflow of the new viewport is blitted rather than leaving the stale frame on screen.
        let settle = self.resize_settle.get();
        if settle > 0 {
            self.resize_settle.set(settle - 1);
            self.window.request_redraw();
        }
    }

    /// Render one pane group's active content into `rect`: (re)size its webviews to the rect,
    /// then either paint the native new-tab dashboard or blit its live page FBO. Shared by the
    /// single view and each half of a split — each pane has its own offscreen context, so two
    /// can show at once.
    fn render_pane(&self, ctx: &egui::Context, pane: usize, rect: egui::Rect, scale: f32) {
        let _ = self.pane(pane).context.make_current();
        let w = (rect.width() * scale).round().max(1.0) as u32;
        let h = (rect.height() * scale).round().max(1.0) as u32;
        if (w, h) != self.pane(pane).content_px.get() {
            self.pane(pane).content_px.set((w, h));
            self.pane(pane).context.resize(PhysicalSize::new(w, h));
            for t in self.pane(pane).tabs.borrow().iter() {
                t.webview.resize(PhysicalSize::new(w, h));
            }
            // Servo reflows the new size ASYNCHRONOUSLY, and notify_new_frame_ready only fires when
            // its event loop is pumped — after a resize nothing reliably pumps it, so the stale
            // (old-size) frame stays on screen until the user interacts. Keep redrawing for a short
            // settle window so the reflowed frame gets blitted. Covers initial sizing + every resize.
            self.resize_settle.set(24);
        }
        // An empty pane (its last tab was closed in a split) paints a clean themed blank rather
        // than blitting whatever stale frame is still in its FBO.
        if self.pane(pane).tabs.borrow().get(self.pane(pane).active.get()).is_none() {
            let bg = self.browser.settings.borrow().theme.palette().bg;
            ctx.layer_painter(LayerId::background()).rect_filled(rect, 0.0, bg);
            return;
        }
        // Blit the live Servo page FBO. The new-tab page is now the HTML `gator://welcome`
        // document (rendered by the engine), so it takes this same path — no native dashboard.
        if let Some(t) = self.pane(pane).tabs.borrow().get(self.pane(pane).active.get()) {
            t.webview.paint();
        }
        // Snapshot-on-switch: periodically grab the just-painted frame into a downscaled thumbnail
        // for the tab-hover preview. Throttled — a glReadPixels every frame would stutter.
        let tick = self.thumb_tick.get().wrapping_add(1);
        self.thumb_tick.set(tick);
        if tick % 24 == 0 {
            self.capture_thumbnail(pane);
        }
        if let Some(blit) = self.pane(pane).context.render_to_parent_callback() {
            ctx.layer_painter(LayerId::background()).add(PaintCallback {
                rect,
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
    }

    /// Read the pane's just-painted FBO into a downscaled ColorImage and stash it on the pane's
    /// active tab (uploaded to a texture next frame). Used for the snapshot-on-switch hover preview.
    fn capture_thumbnail(&self, pane: usize) {
        let (w, h) = self.pane(pane).content_px.get();
        if w == 0 || h == 0 {
            return;
        }
        let _ = self.pane(pane).context.make_current();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(w as i32, h as i32));
        let Some(img) = self.pane(pane).context.read_to_image(rect) else {
            return;
        };
        let (sw, sh) = (img.width() as usize, img.height() as usize);
        let raw = img.as_raw();
        if sw == 0 || sh == 0 || raw.len() < sw * sh * 4 {
            return;
        }
        // Nearest-neighbour downscale to ~320px wide to bound texture memory.
        let tw = sw.min(320);
        let th = ((tw * sh) / sw).max(1);
        let mut bytes = Vec::with_capacity(tw * th * 4);
        for ty in 0..th {
            let sy = ty * sh / th;
            for tx in 0..tw {
                let sx = tx * sw / tw;
                let o = (sy * sw + sx) * 4;
                bytes.extend_from_slice(&raw[o..o + 4]);
            }
        }
        let ci = egui::ColorImage::from_rgba_unmultiplied([tw, th], &bytes);
        let active = self.pane(pane).active.get();
        if let Some(t) = self.pane(pane).tabs.borrow_mut().get_mut(active) {
            t.thumb_pending = Some(ci);
        }
    }

    /// Tab-hover preview: pop the tab's cached snapshot near the hovered tab (below it for the
    /// horizontal strip, to its right for the vertical strip). No-op until the tab has a snapshot.
    fn draw_tab_preview(&self, ui: &egui::Ui, tab_idx: usize, anchor: egui::Rect, vertical: bool) {
        let Some(tex) = self
            .focused_pane()
            .tabs
            .borrow()
            .get(tab_idx)
            .and_then(|t| t.thumb_tex.clone())
        else {
            return;
        };
        let size = tex.size_vec2();
        if size.x < 1.0 {
            return;
        }
        let w = 240.0;
        let h = (w * size.y / size.x).max(1.0);
        let pos = if vertical {
            egui::pos2(anchor.right() + 8.0, anchor.top())
        } else {
            egui::pos2(anchor.left(), anchor.bottom() + 8.0)
        };
        egui::Area::new(egui::Id::new(("tab_preview", tab_idx)))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        tex.id(),
                        egui::vec2(w, h),
                    )));
                });
            });
    }

    /// Focused-pane outline + the close-split button, drawn over a split.
    fn draw_split_overlay(&self, ctx: &egui::Context, left_rect: egui::Rect, right_rect: egui::Rect) {
        let pal = self.browser.settings.borrow().theme.palette();
        let mid = right_rect.left();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("split_divider"),
        ))
        .line_segment(
            [
                egui::pos2(mid, left_rect.top()),
                egui::pos2(mid, left_rect.bottom()),
            ],
            egui::Stroke::new(1.0, pal.border),
        );
        let focused_rect = if self.focused.get() == 1 {
            right_rect
        } else {
            left_rect
        };
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("split_focus"),
        ))
        .rect_stroke(
            focused_rect.shrink(1.0),
            egui::CornerRadius::ZERO,
            egui::Stroke::new(2.0, pal.accent_dim),
            egui::StrokeKind::Inside,
        );
        egui::Area::new(egui::Id::new("close_split"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(right_rect.right() - 30.0, right_rect.top() + 6.0))
            .show(ctx, |ui| {
                // Flat vector × — the UI font renders a "✕" glyph as a .notdef box.
                if icon_button(ui, true, "Close split", &pal, icon::close).clicked() {
                    self.exit_split();
                }
            });
    }

    /// Enter split: open pane 1 (its own webview + history) seeded from `seed`; pane 0 keeps its
    /// page. Focus moves to the new pane.
    fn enter_split(&self, seed: Url) {
        if self.split.get() {
            self.focused.set(1);
            return;
        }
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        let mut builder = WebViewBuilder::new(&self.browser.servo, self.pane1.context.clone())
            .url(seed.clone())
            .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
            .delegate(me);
        let ucm = self.make_tab_ucm();
        if let Some(ucm) = &ucm {
            builder = builder.user_content_manager(ucm.clone());
        }
        let webview = builder.build();
        let (w, h) = self.pane1.content_px.get();
        if w > 0 && h > 0 {
            webview.resize(PhysicalSize::new(w, h));
        }
        webview.show();
        webview.focus();
        self.pane1.tabs.borrow_mut().push(Tab {
            webview,
            url: seed.to_string(),
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
            starred: false,
            ucm,
            injected_addons: RefCell::new(Vec::new()),
            blocked: RefCell::new(Vec::new()),
            audible: Cell::new(false),
            thumb_pending: None,
            thumb_tex: None,
        });
        self.pane1.active.set(0);
        self.split.set(true);
        self.focused.set(1);
        self.window.request_redraw();
    }

    /// Restore a split from a saved session: rebuild pane 1's tabs (its own webviews on the second
    /// FBO) and re-enter split, but land focus on pane 0 (where restore put the primary tabs).
    fn restore_split(&self, urls: Vec<Url>) {
        if urls.is_empty() || self.split.get() {
            return;
        }
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        for url in urls {
            let mut builder = WebViewBuilder::new(&self.browser.servo, self.pane1.context.clone())
                .url(url.clone())
                .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
                .delegate(me.clone());
            let ucm = self.make_tab_ucm();
            if let Some(ucm) = &ucm {
                builder = builder.user_content_manager(ucm.clone());
            }
            let webview = builder.build();
            let (w, h) = self.pane1.content_px.get();
            if w > 0 && h > 0 {
                webview.resize(PhysicalSize::new(w, h));
            }
            self.pane1.tabs.borrow_mut().push(Tab {
                webview,
                url: url.to_string(),
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
                starred: false,
                ucm,
                injected_addons: RefCell::new(Vec::new()),
                blocked: RefCell::new(Vec::new()),
                audible: Cell::new(false),
                thumb_pending: None,
                thumb_tex: None,
            });
        }
        self.pane1.active.set(0);
        self.split.set(true);
        // `focused` is still 0 (pane 0); show pane 1's active tab WITHOUT stealing focus.
        self.select_tab(1, 0);
        self.window.request_redraw();
    }

    /// Exit split: close pane 1's webviews, return focus to pane 0.
    fn exit_split(&self) {
        if !self.split.get() {
            return;
        }
        self.pane1.tabs.borrow_mut().clear();
        self.pane1.active.set(0);
        self.split.set(false);
        self.focused.set(0);
        if let Some(wv) = self.active_tab() {
            wv.show();
            wv.focus();
        }
        self.window.request_redraw();
    }

    /// Toggle split (Ctrl+\\): seed pane 1 from the current page.
    fn toggle_split(&self) {
        if self.split.get() {
            self.exit_split();
        } else {
            let seed = self
                .pane0
                .tabs
                .borrow()
                .get(self.pane0.active.get())
                .and_then(|t| Url::parse(&t.url).ok())
                .unwrap_or_else(content_url);
            self.enter_split(seed);
        }
    }

    fn paint(&self) {
        let _ = self.content_context.make_current();
        self.window_context.prepare_for_rendering();
        self.egui.borrow_mut().paint(&self.window);
        self.round_corners_gl();
        self.window_context.present();
    }

    /// Erase the four window corners to transparent so the undecorated window reads as rounded.
    /// Runs after egui paints and before present, on the current (content) context's surface.
    /// Lazily builds the GL program once; if the build fails, the corners just stay square.
    fn round_corners_gl(&self) {
        use glow::HasContext as _;
        let gl = self.content_context.glow_gl_api();
        if self.corner_mask.borrow().is_none() {
            let built = CornerMaskGl::build(&gl);
            *self.corner_mask.borrow_mut() = Some(built);
        }
        let guard = self.corner_mask.borrow();
        let Some(Some(mask)) = guard.as_ref() else {
            return;
        };
        let size = self.window.inner_size();
        let (w, h) = (size.width.max(1) as i32, size.height.max(1) as i32);
        let radius = (12.0 * self.window.scale_factor() as f32).max(1.0);
        unsafe {
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::CULL_FACE);
            gl.enable(glow::BLEND);
            gl.blend_equation(glow::FUNC_ADD);
            gl.blend_func_separate(
                glow::ZERO,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ZERO,
                glow::ONE_MINUS_SRC_ALPHA,
            );
            gl.viewport(0, 0, w, h);
            gl.use_program(Some(mask.program));
            gl.uniform_2_f32(Some(&mask.u_res), w as f32, h as f32);
            gl.uniform_1_f32(Some(&mask.u_radius), radius);
            gl.bind_vertex_array(Some(mask.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }

    /// Apply the live customization theme to the egui chrome each frame: OKLCH
    /// base/accent -> [`egui::Visuals`], density/font/radius -> [`egui::Style`].
    fn apply_theme(&self, ctx: &egui::Context) {
        let th = self.browser.settings.borrow().theme;
        let pal = th.palette();
        let mode = if th.base.is_light() {
            egui::Theme::Light
        } else {
            egui::Theme::Dark
        };
        ctx.set_theme(mode);
        ctx.set_visuals_of(mode, theme::build_visuals(&th, &pal));
        theme::apply_style(ctx, &th);
    }

    /// Execute a command-palette action: mutate theme/modules/tabs/studio, persist, repaint.
    fn run_palette(&self, action: palette::PaletteAction) {
        use palette::PaletteAction as A;
        match action {
            A::NewTab => {
                self.new_tab(content_url());
                return;
            }
            A::ToggleStudio => {
                // The Studio is now the Appearance section of gator://settings.
                self.navigate_from_omnibox("gator://settings#appearance");
                return;
            }
            A::OpenWhy => {
                self.open_why();
                return;
            }
            A::OpenExport => {
                if let Ok(u) = Url::parse("gator://export") {
                    self.new_tab(u);
                }
                return;
            }
            A::ToggleForceDark => {
                self.toggle_force_dark();
                return;
            }
            A::ReaderMode => {
                self.activate_reader_mode();
                return;
            }
            _ => {}
        }
        {
            let mut s = self.browser.settings.borrow_mut();
            match action {
                A::ToggleVerticalTabs => {
                    s.theme.tab_pos = match s.theme.tab_pos {
                        theme::TabPos::Top => theme::TabPos::Left,
                        theme::TabPos::Left => theme::TabPos::Top,
                    };
                }
                A::ShrinkTabs => {
                    s.theme.tab_fit = match s.theme.tab_fit {
                        theme::TabFit::Fill => theme::TabFit::Fit,
                        theme::TabFit::Fit => theme::TabFit::Fill,
                    };
                }
                A::Density(d) => s.theme.density = d,
                A::SetAccent(a) => s.theme.accent = a,
                A::SetWallpaper(w) => s.theme.wallpaper = w,
                A::ApplyPreset(p) => p.merge_into(&mut s.theme),
                A::ToggleNotes => s.modules.notes = !s.modules.notes,
                A::ToggleFeed => s.modules.feed = !s.modules.feed,
                A::NewTab
                | A::ToggleStudio
                | A::OpenWhy
                | A::OpenExport
                | A::ToggleForceDark
                | A::ReaderMode => {}
            }
            sync_legacy_theme(&mut s);
            save_settings(&s);
        }
        self.window.request_redraw();
    }

    /// Upload any decoded favicons to GPU textures (needs the egui Context, so done here).
    fn load_favicons(&self, ctx: &egui::Context) {
        for (i, tab) in self.focused_pane().tabs.borrow_mut().iter_mut().enumerate() {
            if let Some(img) = tab.favicon_pending.take() {
                tab.favicon_tex =
                    Some(ctx.load_texture(format!("favicon-{i}"), img, Default::default()));
            }
            if let Some(img) = tab.thumb_pending.take() {
                tab.thumb_tex =
                    Some(ctx.load_texture(format!("thumb-{i}"), img, egui::TextureOptions::LINEAR));
            }
        }
    }

    /// Toolbar (nav + address + window controls) and the tab strip.
    fn draw_chrome(&self, ctx: &egui::Context) {
        let frame = egui::Frame::default()
            .fill(ctx.global_style().visuals.window_fill)
            .corner_radius(egui::CornerRadius { nw: 12, ne: 12, sw: 0, se: 0 })
            // Zero bottom margin: any bottom margin here is toolbar `bg2` sitting *below* the omnibar
            // and *above* the tab panel — a dark strip that, against the tint of the tab pills, reads
            // as a "dark region on the buttons" (invisible over the bare strip, bg2-on-bg2). With 0
            // the pills butt straight against the omnibar. Keep 6 up top (window rounding).
            .inner_margin(egui::Margin {
                left: 6,
                right: 6,
                top: 6,
                bottom: 0,
            });
        let toolbar = egui::TopBottomPanel::top("toolbar")
            .frame(frame)
            .show_separator_line(false)
            .show(ctx, |ui| {
            // Pin the row to the omnibar height (the tallest control) up front, so every item —
            // which egui lays out left-to-right and would otherwise top-align before the omnibar
            // grows the row — is vertically centered against it instead of floating too high.
            let omni_h = theme::density_tokens(self.browser.settings.borrow().theme.density).omni_h;
            ui.horizontal(|ui| {
                ui.set_min_height(omni_h);
                let navpal = self.browser.settings.borrow().theme.palette();
                let (cb, cf) = self.active_nav();
                if icon_button(ui, cb, "Back", &navpal, icon::back).clicked() {
                    if let Some(t) = self.active_tab() {
                        t.go_back(1);
                    }
                }
                if icon_button(ui, cf, "Forward", &navpal, icon::forward).clicked() {
                    if let Some(t) = self.active_tab() {
                        t.go_forward(1);
                    }
                }
                if icon_button(ui, true, "Reload", &navpal, icon::reload).clicked() {
                    if let Some(t) = self.active_tab() {
                        t.reload();
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pal = self.browser.settings.borrow().theme.palette();
                    if icon_button(ui, true, "Close", &pal, icon::close).clicked() {
                        // Close THIS window (quits the app only if it's the last one).
                        self.wants_close.set(true);
                        self.window.request_redraw();
                    }
                    let maxed = self.window.is_maximized();
                    if icon_button(ui, true, if maxed { "Restore" } else { "Maximize" }, &pal, |p, r, c| {
                        icon::maximize(p, r, c, maxed)
                    })
                    .clicked()
                    {
                        self.window.set_maximized(!maxed);
                    }
                    if icon_button(ui, true, "Minimize", &pal, icon::minimize).clicked() {
                        self.window.set_minimized(true);
                    }
                    // The ONLY draggable region: a reserved handle just left of the window controls
                    // (browser-style). Recorded so the winit hit-test drags here and nowhere else.
                    let (drag_handle, _) =
                        ui.allocate_exact_size(egui::vec2(40.0, 24.0), egui::Sense::hover());
                    self.drag_rect.set(drag_handle);
                    if icon_button(ui, true, "Settings", &pal, icon::menu).clicked() {
                        self.navigate_from_omnibox("gator://settings");
                    }
                    if icon_button(ui, true, "Customize appearance", &pal, icon::studio).clicked() {
                        self.navigate_from_omnibox("gator://settings#appearance");
                    }
                    // Add-ons (userscripts) puzzle icon: toggles the registry-driven popover. A
                    // count bubble shows how many enabled add-ons match the current tab's URL.
                    {
                        let active_count = {
                            let cur = self.location.borrow().clone();
                            self.browser.addons.borrow().enabled_matching(&cur).len()
                        };
                        let r = icon_button(ui, true, "Userscripts / add-ons", &pal, icon::addons);
                        self.addon_badge_rect.set(r.rect);
                        if active_count > 0 {
                            let p = ui.painter();
                            let center = r.rect.right_top() + egui::vec2(-5.0, 6.0);
                            p.circle_filled(center, 7.0, pal.accent);
                            p.text(
                                center,
                                egui::Align2::CENTER_CENTER,
                                active_count.to_string(),
                                egui::FontId::proportional(9.0),
                                pal.bg,
                            );
                        }
                        if r.clicked() {
                            self.show_addons.set(!self.show_addons.get());
                        }
                    }
                    if self.browser.password_store.borrow().is_unlocked()
                        && icon_button(ui, true, "Save this page's login", &pal, icon::key).clicked()
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
                    let (pal, omni_h, omni_px, set_radius) = {
                        let s = self.browser.settings.borrow();
                        let tk = theme::density_tokens(s.theme.density);
                        (s.theme.palette(), tk.omni_h, tk.omni_px, s.theme.radius)
                    };
                    // Corners follow the theme's set radius (not a forced full pill), capped at
                    // half the bar height so a large radius can't overshoot into a stadium.
                    let rad = set_radius.min((omni_h * 0.5) as u8);
                    // Ctrl/Cmd+K opens the command palette regardless of focus. Handled here on the
                    // egui side because the winit shortcut handler is bypassed while a text field
                    // (the omnibar) has focus, so Ctrl+K did nothing once the omnibar was focused.
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
                        *loc = ">".to_string();
                        self.location_dirty.set(true);
                        self.focus_omnibox.set(true);
                    }
                    let is_cmd = loc.trim_start().starts_with('>');
                    let focused = ui.memory(|m| m.has_focus(id));

                    // The omnibar pill: leading indicator · input · star · ⌘K keycap.
                    let pill = egui::Frame::NONE
                        .fill(if focused { pal.bg } else { pal.elev })
                        .stroke(egui::Stroke::new(1.0, if focused { pal.accent } else { pal.border }))
                        .corner_radius(egui::CornerRadius::same(rad))
                        .inner_margin(egui::Margin::symmetric(omni_px as i8, 0));

                    let mut star_toggle = false;
                    let pill_inner = pill.show(ui, |ui| {
                        ui.set_min_height(omni_h);
                        ui.horizontal_centered(|ui| {
                            // Leading indicator: command `>` in accent, else the secure dot.
                            let (slot, _) =
                                ui.allocate_exact_size(egui::vec2(16.0, omni_h), egui::Sense::hover());
                            if is_cmd {
                                ui.painter().text(
                                    slot.center(),
                                    egui::Align2::CENTER_CENTER,
                                    ">",
                                    egui::FontId::monospace(13.0),
                                    pal.accent,
                                );
                            } else {
                                ui.painter().circle_filled(
                                    slot.center(),
                                    3.5,
                                    theme::oklch(0.74, 0.15, 145.0),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    // (The ⌘K/Ctrl K keycap hint was removed; the shortcut still works.)
                                    let starred = self
                                        .focused_pane()
                                        .tabs
                                        .borrow()
                                        .get(self.focused_pane().active.get())
                                        .map(|t| t.starred)
                                        .unwrap_or(false);
                                    let (glyph, scol) = if starred {
                                        ("★", pal.accent)
                                    } else {
                                        ("☆", pal.muted)
                                    };
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(glyph).size(15.0).color(scol),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        star_toggle = true;
                                    }
                                    ui.add_sized(
                                        egui::vec2(ui.available_width(), omni_h),
                                        egui::TextEdit::singleline(&mut *loc)
                                            .id(id)
                                            .frame(egui::Frame::NONE)
                                            .vertical_align(egui::Align::Center)
                                            .hint_text("Search, enter URL, or type  >  for commands"),
                                    )
                                },
                            )
                            .inner
                        })
                        .inner
                    });
                    let field = pill_inner.inner;
                    let pill_rect = pill_inner.response.rect;
                    self.omni_rect.set(pill_rect);

                    if field.changed() {
                        self.location_dirty.set(true);
                    }
                    if self.focus_omnibox.take() {
                        field.request_focus();
                    }
                    if field.gained_focus() {
                        if let Some(mut st) = TextEditState::load(ui.ctx(), id) {
                            // In command mode (prefilled `>`) place the cursor at the end;
                            // otherwise select-all so a typed URL replaces the old one.
                            let range = if is_cmd {
                                CCursorRange::two(CCursor::new(loc.len()), CCursor::new(loc.len()))
                            } else {
                                CCursorRange::two(CCursor::new(0), CCursor::new(loc.len()))
                            };
                            st.cursor.set_char_range(Some(range));
                            st.store(ui.ctx(), id);
                        }
                    }
                    if focused {
                        // Accent focus ring (drawn on a foreground layer so it isn't clipped).
                        ui.ctx()
                            .layer_painter(egui::LayerId::new(
                                egui::Order::Foreground,
                                egui::Id::new("omni_ring"),
                            ))
                            .rect_stroke(
                                pill_rect,
                                egui::CornerRadius::same(rad),
                                egui::Stroke::new(3.0, pal.accent_soft),
                                egui::StrokeKind::Outside,
                            );
                    }
                    if star_toggle {
                        if let Some(t) = self.focused_pane().tabs.borrow_mut().get_mut(self.focused_pane().active.get()) {
                            t.starred = !t.starred;
                        }
                        self.window.request_redraw();
                    }

                    let mut go: Option<String> = None;
                    let mut run_action: Option<palette::PaletteAction> = None;
                    let mut switch_tab: Option<usize> = None;
                    let mut start_find: Option<String> = None;
                    let verb = if is_cmd {
                        None
                    } else {
                        omnibar_verb(loc.trim_start())
                    };

                    if let Some((v, rest)) = verb {
                        // Scoped launcher: build matching rows, render a dropdown, defer the action.
                        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let q = rest.to_lowercase();
                        // (label, on-select closure result) collected as owned data so no borrow of
                        // `loc`/tabs/bookmarks is held across the dropdown render.
                        let rows: Vec<(String, OmniPick)> = match v {
                            't' => self
                                .focused_pane()
                                .tabs
                                .borrow()
                                .iter()
                                .enumerate()
                                .filter(|(_, t)| {
                                    q.is_empty()
                                        || t.title.to_lowercase().contains(&q)
                                        || t.url.to_lowercase().contains(&q)
                                })
                                .map(|(i, t)| {
                                    let label = if t.title.trim().is_empty() {
                                        t.url.clone()
                                    } else {
                                        format!("{}  —  {}", t.title, t.url)
                                    };
                                    (label, OmniPick::Tab(i))
                                })
                                .take(8)
                                .collect(),
                            'b' => self
                                .browser
                                .profile
                                .borrow()
                                .bookmarks
                                .iter()
                                .filter(|b| {
                                    q.is_empty()
                                        || b.title.to_lowercase().contains(&q)
                                        || b.url.to_lowercase().contains(&q)
                                })
                                .map(|b| {
                                    let label = if b.title.trim().is_empty() {
                                        b.url.clone()
                                    } else {
                                        format!("{}  —  {}", b.title, b.url)
                                    };
                                    (label, OmniPick::Url(b.url.clone()))
                                })
                                .take(8)
                                .collect(),
                            _ => Vec::new(), // '/' = find: no rows, Enter starts the search
                        };
                        let apply = |pick: &OmniPick,
                                     go: &mut Option<String>,
                                     switch_tab: &mut Option<usize>| match pick {
                            OmniPick::Tab(i) => *switch_tab = Some(*i),
                            OmniPick::Url(u) => *go = Some(u.clone()),
                        };
                        if v == '/' {
                            if enter && !rest.is_empty() {
                                start_find = Some(rest.to_string());
                            }
                        } else {
                            if enter {
                                if let Some((_, pick)) = rows.first() {
                                    apply(pick, &mut go, &mut switch_tab);
                                }
                            }
                            if field.has_focus() && !rows.is_empty() {
                                egui::Area::new(egui::Id::new("omnibox_verb"))
                                    .order(egui::Order::Foreground)
                                    .fixed_pos(pill_rect.left_bottom())
                                    .show(ui.ctx(), |ui| {
                                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                                            ui.set_min_width(pill_rect.width().max(220.0));
                                            for (label, pick) in &rows {
                                                if ui
                                                    .add(
                                                        egui::Button::new(truncate_ellipsis(label, 80))
                                                            .frame(false),
                                                    )
                                                    .clicked()
                                                {
                                                    apply(pick, &mut go, &mut switch_tab);
                                                }
                                            }
                                        });
                                    });
                            }
                        }
                    } else if is_cmd {
                        // Command palette: filter the catalog by the text after `>`.
                        let filter = loc.trim_start().trim_start_matches('>').trim().to_lowercase();
                        let items: Vec<_> = palette::palette_catalog()
                            .into_iter()
                            .filter(|(label, _, _)| label.to_lowercase().contains(&filter))
                            .collect();
                        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            run_action = items.first().map(|(_, _, a)| *a);
                        }
                        // Draw whenever in command mode — NOT gated on field focus — so a click on
                        // a row (which steals focus from the omnibar) is still drawn on the release
                        // frame and dispatches, instead of vanishing before the click lands.
                        if let Some(a) =
                            palette::draw_palette_dropdown(ui, pill_rect, &items, &pal)
                        {
                            run_action = Some(a);
                        }
                        // Escape leaves command mode (closes the palette).
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            loc.clear();
                            self.location_dirty.set(true);
                        }
                    } else {
                        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            go = Some(loc.trim().to_string());
                        }
                        // History-backed autocomplete dropdown under the address bar.
                        if field.has_focus() && !loc.trim().is_empty() {
                            let sugg = suggestions(&self.browser.profile.borrow().history, loc.trim());
                            if !sugg.is_empty() {
                                egui::Area::new(egui::Id::new("omnibox_suggest"))
                                    .order(egui::Order::Foreground)
                                    .fixed_pos(pill_rect.left_bottom())
                                    .show(ui.ctx(), |ui| {
                                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                                            ui.set_min_width(pill_rect.width().max(220.0));
                                            for (url, title) in &sugg {
                                                let label = if title.is_empty() {
                                                    url.clone()
                                                } else {
                                                    format!("{title}  —  {url}")
                                                };
                                                if ui
                                                    .add(
                                                        egui::Button::new(truncate_ellipsis(
                                                            &label, 80,
                                                        ))
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
                    }

                    if let Some(a) = run_action {
                        loc.clear();
                        drop(loc);
                        self.location_dirty.set(false);
                        self.run_palette(a);
                    } else if let Some(target) = go {
                        *loc = target.clone();
                        drop(loc);
                        self.location_dirty.set(false);
                        self.navigate_from_omnibox(&target);
                    } else if let Some(i) = switch_tab {
                        // `t ` verb: jump to an open tab in the focused pane.
                        loc.clear();
                        drop(loc);
                        self.location_dirty.set(false);
                        self.select_tab(self.focused.get(), i);
                    } else if let Some(query) = start_find {
                        // `/` verb: open find-in-page and run the query immediately.
                        loc.clear();
                        drop(loc);
                        self.location_dirty.set(false);
                        *self.find_query.borrow_mut() = query.clone();
                        self.find_open.set(true);
                        self.find_focus.set(true);
                        self.find_run(&query);
                        self.window.request_redraw();
                    }
                });
            });
        });

        // 🧩 add-ons popover (registry-driven), anchored under the toolbar badge.
        self.draw_addons_popover(ctx);

        let vertical = self.browser.settings.borrow().theme.tab_pos == theme::TabPos::Left;
        let mut bottom = if vertical {
            // No top tab strip; the left SidePanel sets `content_left`. The page begins
            // below the toolbar, whose bottom we captured above.
            self.draw_tabs_vertical(ctx);
            toolbar.response.rect.max.y
        } else {
            self.content_left.set(0.0);
            self.draw_tabs_horizontal(ctx)
        };

        // Bookmarks bar (only when there are bookmarks), below the tab strip.
        let have_bookmarks = !self.browser.profile.borrow().bookmarks.is_empty();
        if have_bookmarks {
            let bm = egui::TopBottomPanel::top("bookmarks").show(ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let bms: Vec<(String, String)> = self
                                .browser
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

    /// Render-order tab indices: pinned first (in tab order), then the rest. This is the
    /// shared ordering both tab layouts iterate; underlying indices (used by
    /// select/close/move) are unchanged.
    fn tab_order(&self) -> Vec<usize> {
        let tabs = self.focused_pane().tabs.borrow();
        let n = tabs.len();
        let mut o: Vec<usize> = (0..n).filter(|&i| tabs[i].pinned).collect();
        o.extend((0..n).filter(|&i| !tabs[i].pinned));
        o
    }

    /// Render one tab's favicon/spinner + selectable button (+ trailing × when unpinned),
    /// wired with click→select, middle-click→close, the shared context menu, and
    /// drag-to-reorder. Returns the requested action and the main button's response (whose
    /// rect the strip uses for the insertion indicator + scroll-into-view). `vertical`
    /// selects the row layout (full-width row vs compact horizontal chip) and the
    /// orientation-toggle menu label.
    ///
    /// Both layouts call this so their select/close/menu/pin/drag behavior is identical.
    fn tab_row(
        &self,
        ui: &mut egui::Ui,
        i: usize,
        active: usize,
        vertical: bool,
        fill_width: Option<f32>,
    ) -> (TabAction, egui::Response) {
        let (title, fav_id, loading, pinned) = {
            let tabs = self.focused_pane().tabs.borrow();
            (
                tabs[i].title.clone(),
                tabs[i].favicon_tex.as_ref().map(|t| t.id()),
                tabs[i].loading,
                tabs[i].pinned,
            )
        };
        let (pal, tab_h, rad, fid, tab_max_w) = {
            let s = self.browser.settings.borrow();
            let tk = theme::density_tokens(s.theme.density);
            (
                s.theme.palette(),
                tk.tab_h,
                s.theme.radius_sm(),
                egui::FontId::new(tk.fs, theme::family(s.theme.font)),
                s.theme.tab_max_w as f32,
            )
        };
        let dragging = self.focused_pane().drag_tab.get() == Some(i);
        let opacity = if dragging { 0.45 } else { 1.0 };
        let mut act = TabAction::None;

        // Geometry.
        const PAD: f32 = 11.0;
        const ICON: f32 = 16.0;
        const GAP: f32 = 9.0;
        let close_w = if pinned { 0.0 } else { 18.0 };
        let label = if pinned && !vertical {
            String::new()
        } else {
            title.clone()
        };
        // Cheap width estimate (the text is clipped + the width clamped, so exactness is moot).
        let text_w = if label.is_empty() {
            0.0
        } else {
            label.chars().count() as f32 * fid.size * 0.55
        };
        let content_w = PAD
            + ICON
            + if label.is_empty() { 0.0 } else { GAP + text_w }
            + if close_w > 0.0 { GAP + close_w } else { 0.0 }
            + PAD;
        let width = if vertical {
            ui.available_width().max(ICON + 2.0 * PAD)
        } else if let Some(w) = fill_width {
            w.clamp(ICON + 2.0 * PAD, tab_max_w)
        } else {
            content_w.clamp(ICON + 2.0 * PAD, tab_max_w)
        };

        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(width, tab_h), egui::Sense::click_and_drag());
        let painter = ui.painter().clone();

        // Tab background: active = accent-soft pill; hover = elevated; else transparent.
        let fill = if i == active {
            pal.accent_soft
        } else if resp.hovered() {
            pal.elev
        } else {
            egui::Color32::TRANSPARENT
        };
        painter.rect_filled(rect, egui::CornerRadius::same(rad), fill.gamma_multiply(opacity));

        // Favicon chip: real favicon if loaded, else a stable hue-colored rounded square.
        let fav_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + PAD, rect.center().y - ICON / 2.0),
            egui::vec2(ICON, ICON),
        );
        if let Some(tid) = fav_id.filter(|_| !loading) {
            painter.image(
                tid,
                fav_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE.gamma_multiply(opacity),
            );
        } else {
            let hue = favicon_hue(&title);
            painter.rect_filled(
                fav_rect,
                egui::CornerRadius::same(4),
                theme::oklch(0.62, 0.15, hue).gamma_multiply(opacity),
            );
            painter.rect_stroke(
                fav_rect,
                egui::CornerRadius::same(4),
                egui::Stroke::new(1.0, egui::Color32::from_black_alpha(50)),
                egui::StrokeKind::Inside,
            );
            if let Some(ch) = title.chars().next() {
                painter.text(
                    fav_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    ch.to_uppercase().collect::<String>(),
                    egui::FontId::proportional(9.0),
                    egui::Color32::WHITE.gamma_multiply(opacity),
                );
            }
        }

        // Title, clipped so it never spills past the close button.
        if !label.is_empty() {
            let text_left = fav_rect.right() + GAP;
            let text_right = rect.right() - PAD - if close_w > 0.0 { close_w + GAP } else { 0.0 };
            if text_right > text_left {
                let clip = egui::Rect::from_min_max(
                    egui::pos2(text_left, rect.top()),
                    egui::pos2(text_right, rect.bottom()),
                );
                painter.with_clip_rect(clip).text(
                    egui::pos2(text_left, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    fid.clone(),
                    pal.text.gamma_multiply(opacity),
                );
            }
        }

        // Close button (its own hit-test, painted manually — no layout interference).
        if !pinned {
            let close_rect = egui::Rect::from_center_size(
                egui::pos2(rect.right() - PAD - close_w / 2.0, rect.center().y),
                egui::vec2(close_w, close_w),
            );
            let cr = ui.interact(close_rect, resp.id.with("close"), egui::Sense::click());
            if cr.hovered() {
                painter.rect_filled(
                    close_rect,
                    egui::CornerRadius::same(4),
                    egui::Color32::from_black_alpha(60),
                );
            }
            painter.text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(15.0),
                pal.muted.gamma_multiply(opacity),
            );
            if cr.clicked() {
                act = TabAction::Close(i);
            }
        }

        let tab = resp.on_hover_text(&title);

        // Close takes priority over select if both registered the click.
        if matches!(act, TabAction::None) {
            if tab.clicked() && i != active {
                act = TabAction::Select(i);
            }
            if tab.middle_clicked() && !pinned {
                act = TabAction::Close(i);
            }
        }
        // Drag-reorder: begin on threshold-crossing drag start; the strip paints the
        // insertion indicator + commits the move on release (it knows every tab's rect).
        if tab.drag_started() {
            self.focused_pane().drag_tab.set(Some(i));
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
            if ui
                .button(if vertical { "Horizontal tabs" } else { "Vertical tabs" })
                .clicked()
            {
                menu_act = 5;
            }
            if ui.button("Move tab to new window").clicked() {
                menu_act = 6;
            }
        });
        match menu_act {
            1 => act = TabAction::NewTab,
            2 => act = TabAction::Close(i),
            3 => act = TabAction::CloseOthers(i),
            4 => act = TabAction::TogglePin(i),
            5 => act = TabAction::ToggleOrientation,
            6 => act = TabAction::PopOut(i),
            _ => {}
        }

        (act, tab)
    }

    /// Apply the action a tab row requested. Returns true if it mutated the tab set (so the
    /// caller should stop iterating this frame, matching the old `break` after each mutation).
    fn apply_tab_action(&self, act: TabAction) -> bool {
        match act {
            TabAction::None => false,
            TabAction::Select(i) => {
                self.select_tab(self.focused.get(), i);
                false
            }
            TabAction::Close(i) => {
                self.close_tab(self.focused.get(), i);
                true
            }
            TabAction::CloseOthers(i) => {
                self.close_others(i);
                true
            }
            TabAction::TogglePin(i) => {
                self.toggle_pin(i);
                true
            }
            TabAction::NewTab => {
                self.new_tab(content_url());
                true
            }
            TabAction::ToggleOrientation => {
                {
                    let mut s = self.browser.settings.borrow_mut();
                    s.theme.tab_pos = match s.theme.tab_pos {
                        theme::TabPos::Top => theme::TabPos::Left,
                        theme::TabPos::Left => theme::TabPos::Top,
                    };
                    save_settings(&s);
                }
                self.window.request_redraw();
                true
            }
            TabAction::PopOut(i) => {
                // Queue a new window at this tab's URL, then close the tab here. The event loop
                // (which owns the windows map) creates the window on the next redraw.
                let url = self
                    .focused_pane()
                    .tabs
                    .borrow()
                    .get(i)
                    .and_then(|t| Url::parse(&t.url).ok())
                    .unwrap_or_else(content_url);
                self.browser.pending_windows.borrow_mut().push(url);
                self.close_tab(self.focused.get(), i);
                self.window.request_redraw();
                true
            }
        }
    }

    /// Commit a drag-reorder: translate the rendered slot the pointer is over into an
    /// underlying tab index and move the dragged tab there. `rects` are the rendered tab
    /// rects in `order` sequence; `main` reads the relevant axis (x for horizontal, y for
    /// vertical). Returns the candidate render-slot for the insertion indicator while
    /// dragging (so the caller can paint it), or None when not dragging.
    fn drag_target_slot(
        &self,
        order: &[usize],
        rects: &[egui::Rect],
        pointer: egui::Pos2,
        vertical: bool,
    ) -> Option<usize> {
        let dragged = self.focused_pane().drag_tab.get()?;
        order.iter().position(|&i| i == dragged)?; // dragged tab must be in render order
        // Candidate slot = first tab whose center is past the pointer on the main axis.
        let coord = |r: &egui::Rect| if vertical { r.center().y } else { r.center().x };
        let p = if vertical { pointer.y } else { pointer.x };
        let mut slot = rects.len();
        for (k, r) in rects.iter().enumerate() {
            if p < coord(r) {
                slot = k;
                break;
            }
        }
        // Clamp to the dragged tab's pinned-group (v1: reorder only within the group).
        let (group_lo, group_hi) = {
            let tabs = self.focused_pane().tabs.borrow();
            let lo = order.iter().position(|&i| tabs[i].pinned == tabs[dragged].pinned).unwrap_or(0);
            let hi = order.iter().rposition(|&i| tabs[i].pinned == tabs[dragged].pinned).map(|p| p + 1).unwrap_or(order.len());
            (lo, hi)
        };
        slot = slot.clamp(group_lo, group_hi);
        Some(slot)
    }

    /// Horizontal top tab strip (default). Returns the panel's bottom y (the page top).
    fn draw_tabs_horizontal(&self, ctx: &egui::Context) -> f32 {
        let active = self.focused_pane().active.get();
        let scroll_active = self.focused_pane().scroll_active_into_view.take();
        let order = self.tab_order();
        let mut rects: Vec<egui::Rect> = Vec::with_capacity(order.len());
        let mut pending: Option<TabAction> = None;
        let (fill, tab_max_w, tab_h, set_radius) = {
            let s = self.browser.settings.borrow();
            let tk = theme::density_tokens(s.theme.density);
            (
                s.theme.tab_fit == theme::TabFit::Fill,
                s.theme.tab_max_w as f32,
                tk.tab_h,
                s.theme.radius,
            )
        };
        let n = order.len().max(1) as f32;

        // Frame with 0px top AND bottom margins (any top margin re-forms the dark `bg2` gap over the
        // tabs; any bottom margin is a dead strip between the tabs and the page). exact_height pins
        // the strip to exactly the tab height so the panel doesn't grow past the pills either way.
        let outer = egui::TopBottomPanel::top("tabs")
            .frame(
                egui::Frame::default()
                    .fill(ctx.global_style().visuals.panel_fill)
                    // left: 2 puts the first tab's favicon in the same column as the toolbar's
                    // back/forward icons above (they start at frame left 6 + the button's own
                    // inset), instead of the old dead 8px gutter to the left of the tabs.
                    .inner_margin(egui::Margin {
                        left: 2,
                        right: 8,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .exact_height(tab_h)
            .show_separator_line(false)
            .show(ctx, |ui| {
            // In "fill" mode tabs share the strip width evenly (clamped); "fit" uses content width.
            let fw = if fill {
                Some((((ui.available_width() - 48.0) / n).clamp(90.0, tab_max_w)).max(90.0))
            } else {
                None
            };
            let pal = self.browser.settings.borrow().theme.palette();
            ui.horizontal(|ui| {
                // Disable egui's ScrollArea edge-fade. The strip's scrollbar gutter makes the
                // viewport a hair shorter than the pills, so egui fades their content toward the
                // panel bg over its default 20px — invisible on the bare `bg2` strip but a clear
                // dark ramp down the top ~2/3 of every active/tinted tab ("top half darkened").
                ui.spacing_mut().scroll.fade.strength = 0.0;
                // The scrolled strip: the tabs, then the new-tab `+` right after the last tab
                // (scrollbar hidden — a tab strip shouldn't show one; overflow still wheels).
                egui::ScrollArea::horizontal()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.with_layout(
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                for &i in &order {
                                    let (act, resp) = self.tab_row(ui, i, active, false, fw);
                                    rects.push(resp.rect);
                                    if resp.hovered() && i != active {
                                        self.draw_tab_preview(ui, i, resp.rect, false);
                                    }
                                    if i == active && scroll_active {
                                        resp.scroll_to_me(None);
                                    }
                                    if !matches!(act, TabAction::None) && pending.is_none() {
                                        pending = Some(act);
                                    }
                                }
                                // New-tab button, just right of the last tab: a square cell the
                                // exact height of a tab. The `+` is drawn at the cell center and the
                                // hover fill is a symmetric inset, so its top/bottom/left/right
                                // margins are all equal; the cell is vertically centered in the
                                // strip alongside the tabs (Align::Center), so it lines up with them.
                                ui.add_space(4.0);
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(tab_h, tab_h),
                                    egui::Sense::click(),
                                );
                                let resp = resp.on_hover_text("New tab (Ctrl+T)");
                                let c = if resp.hovered() { pal.text } else { pal.muted };
                                let p = ui.painter();
                                if resp.hovered() {
                                    // Equal inset on all four sides; corner follows the set radius,
                                    // capped so it can't over-round the small square.
                                    let inset = (tab_h * 0.14).round();
                                    let nt_rad = set_radius.min(((tab_h - 2.0 * inset) * 0.5) as u8);
                                    p.rect_filled(
                                        rect.shrink(inset),
                                        egui::CornerRadius::same(nt_rad),
                                        pal.elev,
                                    );
                                }
                                let cc = rect.center();
                                let s = (tab_h * 0.20).round();
                                let st = egui::Stroke::new(1.9, c);
                                p.line_segment(
                                    [egui::pos2(cc.x - s, cc.y), egui::pos2(cc.x + s, cc.y)],
                                    st,
                                );
                                p.line_segment(
                                    [egui::pos2(cc.x, cc.y - s), egui::pos2(cc.x, cc.y + s)],
                                    st,
                                );
                                if resp.clicked() {
                                    pending = Some(TabAction::NewTab);
                                }
                            },
                        );
                    });
            });
        });

        self.handle_drag(ctx, &order, &rects, false);
        if let Some(act) = pending {
            self.apply_tab_action(act);
        }
        outer.response.rect.max.y
    }

    /// Vertical left tab strip (a resizable SidePanel). Sets `content_left` to its width so
    /// the page area shifts right by exactly the panel width.
    fn draw_tabs_vertical(&self, ctx: &egui::Context) {
        let active = self.focused_pane().active.get();
        let scroll_active = self.focused_pane().scroll_active_into_view.take();
        let order = self.tab_order();
        let mut rects: Vec<egui::Rect> = Vec::with_capacity(order.len());
        let mut pending: Option<TabAction> = None;

        let panel = egui::SidePanel::left("tabs_vertical")
            .resizable(true)
            .default_width(200.0)
            .show(ctx, |ui| {
                if ui
                    .add(egui::Button::new("+ New tab").frame(false))
                    .clicked()
                {
                    pending = Some(TabAction::NewTab);
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for &i in &order {
                        let (act, resp) = self.tab_row(ui, i, active, true, None);
                        rects.push(resp.rect);
                        if resp.hovered() && i != active {
                            self.draw_tab_preview(ui, i, resp.rect, true);
                        }
                        if i == active && scroll_active {
                            resp.scroll_to_me(None);
                        }
                        if !matches!(act, TabAction::None) && pending.is_none() {
                            pending = Some(act);
                        }
                    }
                });
            });

        self.content_left.set(panel.response.rect.width());
        self.handle_drag(ctx, &order, &rects, true);
        if let Some(act) = pending {
            self.apply_tab_action(act);
        }
    }

    /// Paint the drag-reorder insertion indicator while a tab is being dragged, and commit
    /// the move on release. Shared by both layouts.
    fn handle_drag(&self, ctx: &egui::Context, order: &[usize], rects: &[egui::Rect], vertical: bool) {
        let Some(dragged) = self.focused_pane().drag_tab.get() else {
            return;
        };
        let pointer = ctx.input(|i| i.pointer.interact_pos());
        let still_dragging = ctx.input(|i| i.pointer.any_down());
        let slot = pointer.and_then(|p| self.drag_target_slot(order, rects, p, vertical));
        if let Some(slot) = slot {
            // Insertion indicator: a thin line at the candidate slot's leading edge.
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("tab_drop_indicator"),
            ));
            let color = ctx.style().visuals.selection.bg_fill;
            if let Some(strip) = rects.first().map(|r| r.union(*rects.last().unwrap())) {
                if vertical {
                    let y = rects.get(slot).map(|r| r.top()).unwrap_or_else(|| {
                        rects.last().map(|r| r.bottom()).unwrap_or(strip.top())
                    });
                    painter.line_segment(
                        [egui::pos2(strip.left(), y), egui::pos2(strip.right(), y)],
                        egui::Stroke::new(2.0, color),
                    );
                } else {
                    let x = rects.get(slot).map(|r| r.left()).unwrap_or_else(|| {
                        rects.last().map(|r| r.right()).unwrap_or(strip.left())
                    });
                    painter.line_segment(
                        [egui::pos2(x, strip.top()), egui::pos2(x, strip.bottom())],
                        egui::Stroke::new(2.0, color),
                    );
                }
            }

            if !still_dragging {
                // Released: translate the render slot into an underlying index and move.
                let dragged_pos = order.iter().position(|&i| i == dragged);
                if let Some(dragged_pos) = dragged_pos {
                    // The slot is an insertion point in render order; map it to an
                    // underlying target index. Account for removing the dragged tab first.
                    let target_render = if slot > dragged_pos { slot - 1 } else { slot };
                    let target_render = target_render.min(order.len().saturating_sub(1));
                    let to = order[target_render];
                    self.move_tab(dragged, to);
                }
                self.focused_pane().drag_tab.set(None);
            }
        } else if !still_dragging {
            self.focused_pane().drag_tab.set(None);
        }
        self.window.request_redraw();
    }

    fn draw_dialogs(&self, ctx: &egui::Context) {
        let mut dialogs = self.dialogs.borrow_mut();
        let mut i = 0;
        while i < dialogs.len() {
            if self.draw_one_dialog(ctx, i, &mut dialogs[i]) {
                i += 1;
            } else {
                dialogs.remove(i);
            }
        }
    }

    /// Render one overlay; returns false when it has been resolved (and should be removed). `idx`
    /// is the dialog's queue position — used to give each window a unique egui Id so two dialogs
    /// of the same kind (two alerts, two auth prompts) don't collide into one ambiguous window.
    fn draw_one_dialog(&self, ctx: &egui::Context, idx: usize, dialog: &mut Dialog) -> bool {
        let center = egui::Align2::CENTER_CENTER;
        let win_id = egui::Id::new(("nav_dialog", idx));
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
                egui::Window::new(title).id(win_id)
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
                egui::Window::new("Authentication required").id(win_id)
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
                egui::Window::new("Select").id(win_id)
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
                egui::Window::new("Choose a color").id(win_id)
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
            Dialog::Permission {
                message,
                origin,
                feature,
                handle,
            } => {
                let mut keep = true;
                let mut grant: Option<bool> = None; // Some(allowed) → persist this (origin,feature)
                egui::Window::new("Permission request").id(win_id)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(message.as_str());
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Allow once").clicked() {
                                if let Some(r) = handle.take() {
                                    r.allow();
                                }
                                keep = false;
                            }
                            if ui.button("Allow always").clicked() {
                                if let Some(r) = handle.take() {
                                    r.allow();
                                }
                                grant = Some(true);
                                keep = false;
                            }
                            if ui.button("Block always").clicked() {
                                if let Some(r) = handle.take() {
                                    r.deny();
                                }
                                grant = Some(false);
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
                // Persist an "always" choice outside the closure (avoids borrowing self in it).
                if let Some(allowed) = grant {
                    self.browser
                        .permission_grants
                        .borrow_mut()
                        .insert((origin.clone(), feature.clone()), allowed);
                    save_permission_grants(&self.browser.permission_grants.borrow());
                }
                keep
            }
            Dialog::AddonConsent {
                addon_id,
                name,
                version,
                sites_human,
                perms_human,
            } => {
                let mut keep = true;
                egui::Window::new("Enable userscript?").id(win_id)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(center, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(
                            egui::RichText::new(format!("{name}  v{version}")).strong(),
                        );
                        ui.add_space(6.0);
                        ui.label("Runs on:");
                        for s in sites_human.iter() {
                            ui.label(egui::RichText::new(format!("  • {s}")).monospace());
                        }
                        if !perms_human.is_empty() {
                            ui.add_space(4.0);
                            ui.label("Capabilities:");
                            for p in perms_human.iter() {
                                ui.label(format!("  • {p}"));
                            }
                        }
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "⚠ Runs as code on those pages — it is not sandboxed from them.",
                            )
                            .weak(),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Enable").clicked() {
                                self.set_addon_consent(addon_id, true);
                                keep = false;
                            }
                            if ui.button("Cancel").clicked() {
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
        {
            let mut dialogs = self.dialogs.borrow_mut();
            // Cap the page-driven queue so a misbehaving page (`for(;;)alert()`) can't grow it
            // unbounded or make the render loop draw hundreds of overlay windows per frame.
            if dialogs.len() >= 32 {
                return;
            }
            dialogs.push(d);
        }
        self.window.request_redraw();
    }

    // ── Userscripts / add-ons ─────────────────────────────────────────────────

    /// Persist the add-on registry to disk (best-effort, matches save_history/save_passwords).
    fn save_addons(&self) {
        if let Some(p) = addons_path() {
            let _ = self.browser.addons.borrow().save(p);
        }
    }

    /// Enable or disable an add-on and persist. On enable, copy the full `requested` set into
    /// `granted` (design §6 — Allow grants everything the script asked for; subset grants are a
    /// later refinement). Drops the registry borrow before persisting/redraw.
    fn set_addon_consent(&self, id: &userscripts::AddonId, enable: bool) {
        {
            let mut reg = self.browser.addons.borrow_mut();
            if let Some(a) = reg.get_mut(id) {
                a.enabled = enable;
                if enable {
                    a.granted = a.requested.clone();
                }
            }
        }
        self.save_addons();
        self.window.request_redraw();
    }

    /// Remove an add-on from the registry (consent state only — the *.user.js file on disk is
    /// left untouched, so a re-scan would re-discover it disabled-pending-consent).
    fn remove_addon(&self, id: &userscripts::AddonId) {
        {
            let mut reg = self.browser.addons.borrow_mut();
            reg.remove(id);
        }
        self.save_addons();
        self.window.request_redraw();
    }

    /// Queue an install-consent dialog for any installed-but-not-yet-decided add-on (disabled +
    /// empty grant + non-empty requested OR simply never consented). Called once after startup so
    /// new userscripts surface their consent prompt. Skips add-ons already enabled, and ones that
    /// request no capabilities and are bare-legacy (those auto-enable in `load_addons`).
    fn prompt_pending_consents(&self) {
        let pending: Vec<Dialog> = {
            let reg = self.browser.addons.borrow();
            reg.addons
                .iter()
                .filter(|a| !a.enabled && a.granted.is_empty())
                .map(|a| Dialog::AddonConsent {
                    addon_id: a.id.clone(),
                    name: a.name.clone(),
                    version: a.version.clone(),
                    sites_human: a
                        .matches
                        .iter()
                        .map(describe_match_pattern)
                        .collect::<Vec<_>>(),
                    perms_human: a.requested.iter().map(|p| p.describe().to_string()).collect(),
                })
                .collect()
        };
        for d in pending {
            self.push_dialog(d);
        }
    }

    /// Per-site userscript injection (design §4 Option A). For the navigated `url`, find the
    /// enabled add-ons whose `@match` accepts it (and `@exclude` doesn't), wrap each via
    /// `userscripts::wrap_userscript` with a per-injection capability token bound to the add-on
    /// id, and reconcile this tab's `UserContentManager` against the URL being navigated to:
    /// `remove_script` any already-injected add-on whose `@match`/`@include` no longer applies, and
    /// `add_script` newly-matching ones (skipping any already injected). Called from
    /// `request_navigation` *before* the load is allowed, so the change takes effect on that load
    /// (Servo applies UCM edits on the *next* load). This fixes the old accumulation bug (LYK-1256):
    /// a script attached on site A used to keep running after the tab navigated to a non-matching
    /// site B, because the engine had no remove primitive — it does now.
    fn inject_userscripts(&self, webview: &WebView, url: &str) {
        let Some((pane, tab_idx)) = self.locate_tab(webview) else {
            return;
        };
        // Collect the wrapped scripts to add (id + js) and the set of ids that match this URL,
        // reading source files outside the tab borrow.
        let salt = self.browser.gm_salt; // [u8; 16] is Copy — avoids borrowing self in the closure.
        let (matching_ids, to_add): (Vec<userscripts::AddonId>, Vec<(userscripts::AddonId, String)>) = {
            let reg = self.browser.addons.borrow();
            let already: Vec<userscripts::AddonId> = {
                let tabs = self.pane(pane).tabs.borrow();
                match tabs.get(tab_idx) {
                    Some(t) => t.injected_addons.borrow().iter().map(|(id, _)| id.clone()).collect(),
                    None => return,
                }
            };
            let matching = reg.enabled_matching(url);
            let matching_ids: Vec<userscripts::AddonId> =
                matching.iter().map(|a| a.id.clone()).collect();
            let to_add = matching
                .into_iter()
                .filter(|a| !already.contains(&a.id))
                .filter_map(|a| {
                    // Refutable destructure (`else`) so adding AddonSource::WebExtension later is
                    // additive, not a compile break here (forward-compat, design §8). The `else`
                    // is unreachable while Userscript is the only variant — allow that until then.
                    #[allow(irrefutable_let_patterns)]
                    let userscripts::AddonSource::Userscript { path, .. } = &a.source else {
                        return None;
                    };
                    let src = std::fs::read_to_string(path).ok()?;
                    let cap = addon_cap_token(&salt, &a.id);
                    Some((a.id.clone(), userscripts::wrap_userscript(a, &src, &cap)))
                })
                .collect();
            (matching_ids, to_add)
        };
        let tabs = self.pane(pane).tabs.borrow();
        let Some(tab) = tabs.get(tab_idx) else {
            return;
        };
        let Some(ucm) = &tab.ucm else {
            return;
        };
        let mut injected = tab.injected_addons.borrow_mut();
        // Drop scripts whose @match no longer applies to this navigation, so a site-A script stops
        // running once the tab moves to a non-matching site B (LYK-1256).
        injected.retain(|(id, rc)| {
            if matching_ids.contains(id) {
                true
            } else {
                ucm.remove_script(rc.clone());
                false
            }
        });
        // Add newly-matching scripts, retaining each Rc so it can be removed on a later navigation.
        for (id, js) in to_add {
            let script = Rc::new(UserScript::new(js, None));
            ucm.add_script(script.clone());
            injected.push((id, script));
        }
    }

    /// Handle a `navgator://gm/<cap>/<call>?a=<json-args>` bridge request (design §5). Returns the
    /// JSON response body bytes. Validates `<cap>` (the per-process secret token, see
    /// `addon_cap_token`) against the registry to find the calling add-on, then checks the add-on's
    /// `granted` permission for the capability before performing it.
    ///
    /// Call arguments travel in the URL `?a=` query (URL-encoded JSON) rather than a request body,
    /// because Servo's `WebResourceRequest` exposes only method/headers/url to `load_web_resource`
    /// — never the body. `storage.{list,set,get,delete}` are fully implemented here (local per-add-on
    /// JSON store). `net.fetch` enforces the capability AND the `@connect` host allow-list, but the
    /// actual cross-origin fetch is not yet wired (needs an async HTTP client off the UI thread) and
    /// returns `net-fetch-unimplemented` once it passes the gates. `notify.show`/`tabs.open`/
    /// `clipboard.set` are gated but not yet performed (`not-implemented`).
    ///
    /// TRANSPORT (verified on a live run): Servo delivers neither a page `fetch()` NOR an XHR of the
    /// custom `navgator://` scheme to this intercept (both throw NetworkError) — only a subresource
    /// `<img>` beacon (or a top-level navigation) reaches it. So `wrap_userscript` now fires calls via
    /// an Image beacon: the side effect below runs, but the returned body is DISCARDED by the page.
    /// Fire-and-forget calls (`storage.set/delete`, `notify`, `tabs.open`, `clipboard.set`) work;
    /// data-returning calls (`storage.get/list`, `net.fetch`) need a native `evaluate_javascript`
    /// push-path back into the page — not yet built (issue #4). The capability gate holds regardless.
    fn handle_gm_bridge(&self, url: &Url) -> Vec<u8> {
        use userscripts::Permission;
        let deny = |msg: &str| -> Vec<u8> {
            format!("{{\"ok\":false,\"error\":\"{msg}\"}}").into_bytes()
        };
        // Path is `/<cap>/<call>` (host is "gm"). path_segments splits on '/'.
        let mut segs = match url.path_segments() {
            Some(s) => s,
            None => return deny("bad-path"),
        };
        let cap = match segs.next() {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return deny("no-cap"),
        };
        let call = segs.next().unwrap_or("").to_string();

        // Call args: URL-encoded JSON in the `a` query param (see wrap_userscript's __ep).
        let args: serde_json::Value = url
            .query_pairs()
            .find(|(k, _)| &**k == "a")
            .and_then(|(_, v)| serde_json::from_str(&v).ok())
            .unwrap_or(serde_json::Value::Null);
        let arg_key = || args.get("key").and_then(|v| v.as_str()).map(str::to_string);

        // Resolve cap → add-on via the per-process secret salt (unforgeable from a page's public
        // view of the add-on id; design §5/§11). Clone the bits we need so we don't hold the
        // registry borrow across the action.
        let salt = self.browser.gm_salt;
        let resolved = {
            let reg = self.browser.addons.borrow();
            reg.addons
                .iter()
                .find(|a| a.enabled && addon_cap_token(&salt, &a.id) == cap)
                .map(|a| (a.id.clone(), a.granted.clone(), a.connect.clone()))
        };
        let Some((id, granted, connect)) = resolved else {
            return deny("bad-cap");
        };

        match call.as_str() {
            "storage.list" => {
                if !granted.contains(Permission::Storage) {
                    return deny("permission-denied");
                }
                let keys = addon_storage_load(&id);
                let list: Vec<String> = keys.keys().cloned().collect();
                serde_json::to_vec(&serde_json::json!({ "ok": true, "keys": list }))
                    .unwrap_or_else(|_| deny("serialize"))
            }
            "storage.get" => {
                if !granted.contains(Permission::Storage) {
                    return deny("permission-denied");
                }
                let Some(key) = arg_key() else { return deny("bad-args") };
                let map = addon_storage_load(&id);
                let resp = match map.get(&key) {
                    Some(v) => serde_json::json!({ "ok": true, "value": v }),
                    None => serde_json::json!({ "ok": true }),
                };
                serde_json::to_vec(&resp).unwrap_or_else(|_| deny("serialize"))
            }
            "storage.set" => {
                if !granted.contains(Permission::Storage) {
                    return deny("permission-denied");
                }
                let Some(key) = arg_key() else { return deny("bad-args") };
                let value = args.get("value").cloned().unwrap_or(serde_json::Value::Null);
                let mut map = addon_storage_load(&id);
                map.insert(key, value);
                if addon_storage_save(&id, &map).is_err() {
                    return deny("storage-write");
                }
                b"{\"ok\":true}".to_vec()
            }
            "storage.delete" => {
                if !granted.contains(Permission::Storage) {
                    return deny("permission-denied");
                }
                let Some(key) = arg_key() else { return deny("bad-args") };
                let mut map = addon_storage_load(&id);
                map.remove(&key);
                if addon_storage_save(&id, &map).is_err() {
                    return deny("storage-write");
                }
                b"{\"ok\":true}".to_vec()
            }
            "net.fetch" => {
                if !granted.contains(Permission::CrossOriginFetch) {
                    return deny("permission-denied");
                }
                // Enforce the @connect host allow-list — without this the one genuinely
                // CORS-bypassing capability would be an unrestricted exfil primitive.
                let Some(target) = args.get("url").and_then(|v| v.as_str()) else {
                    return deny("bad-args");
                };
                let host = Url::parse(target).ok().and_then(|u| u.host_str().map(str::to_string));
                let Some(host) = host else { return deny("bad-url") };
                if !connect_allows(&connect, &host) {
                    return deny("connect-not-allowed");
                }
                // Gate passed; the actual cross-origin fetch is not yet wired (needs async HTTP).
                deny("net-fetch-unimplemented")
            }
            "notify.show" => {
                if !granted.contains(Permission::Notifications) {
                    return deny("permission-denied");
                }
                deny("not-implemented")
            }
            "tabs.open" => {
                if !granted.contains(Permission::TabControl) {
                    return deny("permission-denied");
                }
                deny("not-implemented")
            }
            "clipboard.set" => {
                if !granted.contains(Permission::Clipboard) {
                    return deny("permission-denied");
                }
                deny("not-implemented")
            }
            _ => deny("unknown-call"),
        }
    }

    /// The 🧩 add-ons popover: a theme-matched panel under the toolbar badge listing every
    /// installed add-on with an "active on this page" dot and a quick enable/disable toggle, plus
    /// a footer link to the full manager (gator://extensions). Toggled by `show_addons`; dismissed
    /// on outside click (context-menu pattern). Registry-driven, so it's kind-agnostic.
    fn draw_addons_popover(&self, ctx: &egui::Context) {
        if !self.show_addons.get() {
            return;
        }
        let anchor = self.addon_badge_rect.get();
        if anchor == egui::Rect::NOTHING {
            return;
        }
        let pal = self.browser.settings.borrow().theme.palette();
        let cur_url = self.location.borrow().clone();

        // Snapshot the rows to draw (avoid holding the registry borrow across UI/actions).
        let rows: Vec<(userscripts::AddonId, String, bool, bool)> = {
            let reg = self.browser.addons.borrow();
            let active: std::collections::HashSet<userscripts::AddonId> = reg
                .enabled_matching(&cur_url)
                .into_iter()
                .map(|a| a.id.clone())
                .collect();
            reg.addons
                .iter()
                .map(|a| (a.id.clone(), a.name.clone(), a.enabled, active.contains(&a.id)))
                .collect()
        };

        let mut toggle: Option<(userscripts::AddonId, bool)> = None;
        let mut open_manager = false;
        let r = egui::Area::new(egui::Id::new("addons_popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 6.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(pal.bg2)
                    .stroke(egui::Stroke::new(1.0, pal.border))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_min_width(240.0);
                        ui.label(egui::RichText::new("Userscripts").strong().color(pal.text));
                        ui.add_space(4.0);
                        if rows.is_empty() {
                            ui.label(
                                egui::RichText::new("No userscripts installed.")
                                    .small()
                                    .color(pal.muted),
                            );
                        }
                        for (id, name, enabled, active) in rows.iter() {
                            ui.horizontal(|ui| {
                                let dot = if *active { pal.accent } else { pal.border };
                                let (slot, _) = ui.allocate_exact_size(
                                    egui::vec2(12.0, 16.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().circle_filled(slot.center(), 3.5, dot);
                                ui.label(
                                    egui::RichText::new(truncate_ellipsis(name, 28)).color(pal.text),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let label = if *enabled { "On" } else { "Off" };
                                        if ui
                                            .add(egui::Button::new(label).frame(false))
                                            .clicked()
                                        {
                                            toggle = Some((id.clone(), !*enabled));
                                        }
                                    },
                                );
                            });
                        }
                        ui.add_space(6.0);
                        ui.separator();
                        if ui
                            .add(egui::Button::new("Manage add-ons…").frame(false))
                            .clicked()
                        {
                            open_manager = true;
                        }
                    });
            });

        if let Some((id, enable)) = toggle {
            self.set_addon_consent(&id, enable);
        }
        if open_manager {
            self.show_addons.set(false);
            if let Some(tab) = self.active_tab() {
                if let Ok(u) = Url::parse("gator://extensions") {
                    tab.load(u);
                }
            }
        }
        if r.response.clicked_elsewhere() {
            self.show_addons.set(false);
        }
    }

    fn navigate_from_omnibox(&self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let target = omnibox_target(raw, &self.browser.settings.borrow().search);
        if let (Ok(url), Some(tab)) = (Url::parse(&target), self.active_tab()) {
            self.location_dirty.set(false);
            // Reconcile this tab's userscripts BEFORE the load starts. The omnibox uses `tab.load`,
            // which bypasses `request_navigation`, so without this a script scoped to the old site
            // would linger for one more load (UCM edits apply on the *next* load); doing it here
            // removes it in time for this navigation (LYK-1256).
            self.inject_userscripts(&tab, url.as_str());
            tab.load(url);
        }
    }

    // ── Page zoom ──────────────────────────────────────────────────────────────
    fn active_zoom(&self) -> f32 {
        self.focused_pane().tabs
            .borrow()
            .get(self.focused_pane().active.get())
            .map(|t| t.zoom)
            .unwrap_or(1.0)
    }

    fn apply_zoom(&self, zoom: f32) {
        let z = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if let Some(tab) = self.focused_pane().tabs.borrow_mut().get_mut(self.focused_pane().active.get()) {
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
    /// Build a fresh, empty per-tab `UserContentManager` to attach at WebView-build time.
    /// Returns `None` when no add-ons are installed (so a script-less profile keeps the old
    /// no-UCM behavior). The UCM starts empty; `inject_userscripts` later `add_script`s the
    /// add-ons whose `@match` accepts each navigated URL (the engine captures the UCM at build
    /// time and has no post-build setter, so we keep this Rc per tab and grow it on navigation).
    fn make_tab_ucm(&self) -> Option<Rc<UserContentManager>> {
        // The UCM is now always created: it carries the always-on Web Animations API polyfill
        // (LYK-1411) so `element.animate(...)` actually animates until the engine's WAAPI is
        // complete. Userscripts + the DOM probe stack on top of it.
        let ucm = Rc::new(UserContentManager::new(&self.browser.servo));
        // Runs at document-start (before page scripts), so its `Element.prototype.animate` is in
        // place when a page calls it; it self-gates off if native WAAPI is functional.
        ucm.add_script(Rc::new(UserScript::new(WAAPI_POLYFILL_JS.to_string(), None)));
        ucm.add_script(Rc::new(UserScript::new(RIC_POLYFILL_JS.to_string(), None)));
        ucm.add_script(Rc::new(UserScript::new(PLATFORM_POLYFILLS_JS.to_string(), None)));
        if std::env::var_os("NAVGATOR_DOMPROBE").is_some() {
            // Diagnostic: run the DOM probe at document-start so its +3s/+6s snapshots fire
            // even when the page never reaches LoadStatus::Complete.
            ucm.add_script(Rc::new(UserScript::new(DOM_PROBE_JS.to_string(), None)));
        }
        Some(ucm)
    }

    fn new_tab(&self, url: Url) {
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        let ucm = self.make_tab_ucm();
        let mut builder = WebViewBuilder::new(&self.browser.servo, self.focused_pane().context.clone())
            .url(url)
            .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
            .delegate(me);
        if let Some(ucm) = &ucm {
            builder = builder.user_content_manager(ucm.clone());
        }
        let webview = builder.build();
        self.adopt_tab(webview, ucm);
    }

    fn adopt_tab(&self, webview: WebView, ucm: Option<Rc<UserContentManager>>) {
        let (w, h) = self.focused_pane().content_px.get();
        if w > 0 && h > 0 {
            webview.resize(PhysicalSize::new(w, h));
        }
        let idx = {
            let mut tabs = self.focused_pane().tabs.borrow_mut();
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
                starred: false,
                ucm,
                injected_addons: RefCell::new(Vec::new()),
                blocked: RefCell::new(Vec::new()),
                audible: Cell::new(false),
                thumb_pending: None,
                thumb_tex: None,
            });
            tabs.len() - 1
        };
        self.select_tab(self.focused.get(), idx);
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
        // Persist pane 0's tabs, then (when split) a `#PANE1` marker + pane 1's tabs, so the split
        // layout restores. Older readers / a non-split session just see a flat URL list — the
        // marker line isn't a valid URL, so it's skipped. No tabs are ever silently lost.
        let p0 = self.pane0.tabs.borrow();
        let p1 = self.pane1.tabs.borrow();
        let mut s = String::new();
        for t in p0.iter().filter(|t| !t.url.is_empty()) {
            s.push_str(&tsv_field(&t.url));
            s.push('\n');
        }
        if self.split.get() && p1.iter().any(|t| !t.url.is_empty()) {
            s.push_str("#PANE1\n");
            for t in p1.iter().filter(|t| !t.url.is_empty()) {
                s.push_str(&tsv_field(&t.url));
                s.push('\n');
            }
        }
        let _ = std::fs::write(path, s);
    }

    fn select_tab(&self, pane: usize, i: usize) {
        // Only the focused pane drives keyboard focus and the address bar; selecting a tab in a
        // background split pane still shows/throttles its webviews but must not steal either.
        let is_focused = pane == self.focused.get();
        {
            let tabs = self.pane(pane).tabs.borrow();
            if i >= tabs.len() {
                return;
            }
            for (j, tab) in tabs.iter().enumerate() {
                if j == i {
                    tab.webview.show();
                    tab.webview.set_throttled(false);
                } else {
                    // Background tabs are hidden (compositor-only, doesn't touch audio) and
                    // throttled for less CPU/battery — EXCEPT audible ones, which stay un-throttled
                    // so background media (playlists / auto-advance) keeps running at full speed.
                    tab.webview.hide();
                    tab.webview.set_throttled(!tab.audible.get());
                }
            }
            if is_focused {
                tabs[i].webview.focus();
                *self.location.borrow_mut() = tabs[i].url.clone();
            }
        }
        if is_focused {
            self.location_dirty.set(false);
        }
        self.pane(pane).active.set(i);
        self.pane(pane).scroll_active_into_view.set(true);
        self.window.request_redraw();
    }

    fn close_tab(&self, pane: usize, i: usize) {
        {
            let mut tabs = self.pane(pane).tabs.borrow_mut();
            if i >= tabs.len() {
                return;
            }
            let url = tabs[i].url.clone();
            if !url.is_empty() {
                let mut ct = self.closed_tabs.borrow_mut();
                ct.push(url);
                // Bounded "recently closed" stack (reopen with Ctrl+Shift+T).
                let n = ct.len();
                if n > 25 {
                    ct.drain(0..n - 25);
                }
            }
            tabs.remove(i); // dropping the WebView handle closes the webview
        }
        if self.pane(pane).tabs.borrow().is_empty() {
            let pane0_empty = self.pane0.tabs.borrow().is_empty();
            let pane1_empty = self.pane1.tabs.borrow().is_empty();
            if pane0_empty && pane1_empty {
                // Whole window is tabless — close it (the loop quits only if it's the last one).
                self.wants_close.set(true);
                self.window.request_redraw();
            } else if self.split.get() {
                // A split pane emptied but the other still has tabs. Don't leave an empty pane
                // focused (chrome would act on nothing). Exactly one pane survives: if it's pane0,
                // collapse the split onto it; if it's pane1, focus pane1 (can't collapse onto it).
                if pane1_empty {
                    self.exit_split();
                } else {
                    self.focused.set(1);
                    let n = self.pane1.tabs.borrow().len();
                    self.select_tab(1, self.pane1.active.get().min(n - 1));
                }
            }
            return;
        }
        let len = self.pane(pane).tabs.borrow().len();
        let active = self.pane(pane).active.get();
        let new_active = if active >= len {
            len - 1
        } else if active > i {
            active - 1
        } else {
            active
        };
        self.select_tab(pane, new_active);
        self.save_session();
    }

    /// Close every tab except `keep` (tab context menu).
    /// Toggle the pinned state of tab `i`.
    fn toggle_pin(&self, i: usize) {
        if let Some(t) = self.focused_pane().tabs.borrow_mut().get_mut(i) {
            t.pinned = !t.pinned;
        }
        self.window.request_redraw();
    }

    /// Drag-reorder: move the underlying tab at `from` to the position of `to`, preserving
    /// the pinned-group invariant (v1 only reorders within a group) and keeping `active`
    /// on the moved tab.
    fn move_tab(&self, from: usize, to: usize) {
        {
            let mut tabs = self.focused_pane().tabs.borrow_mut();
            if from >= tabs.len() || to >= tabs.len() || from == to {
                return;
            }
            // v1: only reorder within the same pinned group (keeps the pinned-first
            // `order` invariant trivially correct — a tab can't change pinned-state on drop).
            if tabs[from].pinned != tabs[to].pinned {
                return;
            }
            let t = tabs.remove(from);
            tabs.insert(to, t);
        }
        // Fix up the active index so the same tab stays selected after the move.
        let a = self.focused_pane().active.get();
        let new_a = if a == from {
            to
        } else if from < a && a <= to {
            a - 1
        } else if to <= a && a < from {
            a + 1
        } else {
            a
        };
        self.focused_pane().active.set(new_a);
        self.focused_pane().scroll_active_into_view.set(true);
        self.window.request_redraw();
        self.save_session();
    }

    fn close_others(&self, keep: usize) {
        {
            let tabs = self.focused_pane().tabs.borrow();
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
            let mut tabs = self.focused_pane().tabs.borrow_mut();
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
        self.focused_pane().active.set(new_active);
        self.select_tab(self.focused.get(), new_active);
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
        if self.browser.syncing.get() {
            return;
        }
        let (api_key, sync_bookmarks, sync_history, sync_passwords) = {
            let s = self.browser.settings.borrow();
            (
                s.sync_api_key.clone(),
                s.sync_bookmarks,
                s.sync_history,
                s.sync_passwords,
            )
        };
        if api_key.trim().is_empty() {
            *self.browser.sync_status.borrow_mut() = "Set a Lyku API key first.".into();
            return;
        }
        if !sync_bookmarks && !sync_history && !sync_passwords {
            *self.browser.sync_status.borrow_mut() = "Enable a collection to sync first.".into();
            return;
        }
        let snap = {
            let p = self.browser.profile.borrow();
            // Passwords are encrypted HERE (UI thread) — the sync thread only sees ciphertext,
            // and only when the store is unlocked.
            let store = self.browser.password_store.borrow();
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
                cursor_bookmarks: self.browser.sync_cursor_bookmarks.get(),
                cursor_history: self.browser.sync_cursor_history.get(),
                cursor_passwords: self.browser.sync_cursor_passwords.get(),
            }
        };
        self.browser.syncing.set(true);
        *self.browser.sync_status.borrow_mut() = "Syncing…".into();
        let proxy = self.browser.event_proxy.clone();
        std::thread::spawn(move || {
            let outcome = sync::run_sync(snap);
            let _ = proxy.send_event(WakeUp::SyncDone(outcome));
        });
    }

    /// Apply a finished background sync to the local stores (UI thread). Last-write-wins by
    /// `updated`; deletes are not propagated in early access.
    fn apply_sync(&self, outcome: sync::SyncOutcome) {
        self.browser.syncing.set(false);
        *self.browser.sync_status.borrow_mut() = outcome.message.clone();
        if outcome.ok {
            {
                let mut p = self.browser.profile.borrow_mut();
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
            if self.browser.password_store.borrow().is_unlocked() {
                let mut changed = false;
                for pw in &outcome.passwords {
                    if pw.deleted {
                        continue;
                    }
                    if let Some(blob) = hex_decode(&pw.payload) {
                        let cred = self.browser.password_store.borrow().decrypt_credential(&blob);
                        if let Some(cred) = cred {
                            self.browser.password_store.borrow_mut().upsert(cred);
                            changed = true;
                        }
                    }
                }
                if changed {
                    let _ = self.browser.password_store.borrow().save();
                }
            }
            self.browser.sync_cursor_bookmarks.set(outcome.cursor_bookmarks);
            self.browser.sync_cursor_history.set(outcome.cursor_history);
            self.browser.sync_cursor_passwords.set(outcome.cursor_passwords);
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
        let mut p = self.browser.profile.borrow_mut();
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
        let (url, title) = match self.focused_pane().tabs.borrow().get(self.focused_pane().active.get()) {
            Some(t) => (t.url.clone(), t.title.clone()),
            None => return,
        };
        if url.is_empty() {
            return;
        }
        let mut p = self.browser.profile.borrow_mut();
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

    /// Import bookmarks + recent history from installed Chromium-family browsers and Firefox
    /// (deduped by URL). Read-only — the source browsers are never modified.
    fn import_browser_data(&self) {
        let mut bookmarks = import_chrome_bookmarks();
        bookmarks.extend(import_firefox_bookmarks());
        let history = import_browser_history();
        let (mut b_added, mut h_added) = (0usize, 0usize);
        {
            let mut p = self.browser.profile.borrow_mut();
            let mut seen: std::collections::HashSet<String> =
                p.bookmarks.iter().map(|b| b.url.clone()).collect();
            for (url, title) in bookmarks {
                if seen.insert(url.clone()) {
                    p.bookmarks.push(Bookmark {
                        url,
                        title,
                        updated: now_ms(),
                    });
                    b_added += 1;
                }
            }
            for (url, title, visits, updated) in history {
                if let Some(e) = p.history.iter_mut().find(|e| e.url == url) {
                    e.visits = e.visits.max(visits);
                    if e.title.is_empty() {
                        e.title = title;
                    }
                } else {
                    p.history.push(HistoryEntry {
                        url,
                        title,
                        visits,
                        updated,
                    });
                    h_added += 1;
                }
            }
            // Keep history bounded to the most-recent entries.
            const MAX_HISTORY: usize = 3000;
            if p.history.len() > MAX_HISTORY {
                p.history.sort_by(|a, b| b.updated.cmp(&a.updated));
                p.history.truncate(MAX_HISTORY);
            }
            save_bookmarks(&p);
            save_history(&p);
        }
        *self.browser.import_msg.borrow_mut() = Some(if b_added + h_added > 0 {
            format!(
                "Imported {b_added} bookmark(s) + {h_added} history entr{}.",
                if h_added == 1 { "y" } else { "ies" }
            )
        } else {
            "No new bookmarks or history found (Chrome / Firefox / …).".to_string()
        });
        self.window.request_redraw();
    }

    /// Register NavGator as the system default browser (Settings → Setup).
    fn make_default_browser(&self) {
        *self.browser.import_msg.borrow_mut() = Some(set_default_browser());
        self.window.request_redraw();
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
        let result = self.browser.password_store.borrow_mut().unlock(passphrase);
        // On a successful unlock, persist the passphrase to the OS keyring iff the user opted in.
        // This is the only place (besides the checkbox-on path) the plaintext is written out, and
        // it goes nowhere but the OS keyring. Failure is non-fatal — the user can unlock manually
        // next time. The auto-unlock-at-launch path re-enters here, harmlessly re-storing.
        if result.is_ok() && self.browser.settings.borrow().remember_passphrase {
            let _ = keyring_store::store(passphrase);
        }
        let msg = match result {
            Ok(()) => format!(
                "Password store unlocked ({} saved).",
                self.browser.password_store.borrow().len()
            ),
            Err(e) => format!("Unlock failed: {e}"),
        };
        *self.browser.password_msg.borrow_mut() = Some(msg);
        self.window.request_redraw();
    }

    /// Autofill the login form of tab `tab_idx` if the store is unlocked and a saved login
    /// matches the page origin. The credential goes straight from the store into the form via
    /// evaluate_javascript — it is never exposed to page-readable storage.
    fn autofill(&self, pane: usize, tab_idx: usize) {
        if !self.browser.password_store.borrow().is_unlocked() {
            return;
        }
        let (webview, origin) = {
            let tabs = self.pane(pane).tabs.borrow();
            let Some(t) = tabs.get(tab_idx) else {
                return;
            };
            let Some(origin) = origin_of(&t.url) else {
                return;
            };
            (t.webview.clone(), origin)
        };
        let cred = self
            .browser
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

    // (B) AUTO-OFFER-SAVE — deliberately NOT implemented for v1. A *secure* auto-offer is
    // possible (reuse READ_FORM_JS, whose eval RESULT channel never touches page-readable
    // storage), but a *reliable* submit TRIGGER is not cleanly available. The two viable triggers
    // both fail the bar: (1) injecting a submit-listener that polls a page-readable sentinel
    // global is fragile, and the post-submit navigation frequently tears down the form before
    // READ_FORM_JS can run (the chained eval must itself be deferred via the CosmeticReady-style
    // queue, adding a race); (2) reading on load-START of the next page is too late — the
    // credentialed document is already gone. A clean, race-free trigger needs an engine event for
    // form submission (a fork patch surfacing a "form submitted" EmbedderMsg re-exported through
    // navgator-engine), which is out of scope here. So: ship the OS-keyring auto-unlock (A), keep
    // the manual 🔑 save button below, and file the engine-patch follow-up.

    /// Read the active page's login form and save it to the (unlocked) store.
    fn save_login_active(&self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let origin = self
            .focused_pane()
            .tabs
            .borrow()
            .get(self.focused_pane().active.get())
            .and_then(|t| origin_of(&t.url));
        let Some(origin) = origin else {
            *self.browser.password_msg.borrow_mut() = Some("Logins can only be saved on http(s) pages.".into());
            self.window.request_redraw();
            return;
        };
        if !self.browser.password_store.borrow().is_unlocked() {
            *self.browser.password_msg.borrow_mut() =
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
                *me.browser.password_msg.borrow_mut() = Some("No filled password field on this page.".into());
                me.window.request_redraw();
                return;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                let user = v.get("u").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let pass = v.get("p").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !pass.is_empty() {
                    {
                        let mut store = me.browser.password_store.borrow_mut();
                        store.upsert(password::Credential {
                            origin: origin.clone(),
                            username: user,
                            password: pass,
                            updated: now_ms(),
                        });
                        let _ = store.save();
                    }
                    *me.browser.password_msg.borrow_mut() = Some(format!("Saved login for {origin}."));
                    me.window.request_redraw();
                }
            }
        });
    }

    /// Hide ad/clutter elements using EasyList's cosmetic (element-hiding) rules. Two evals:
    /// collect the page's class/id set, then inject a `<style>` hiding the matching selectors —
    /// generic rules are filtered to the page's actual classes/ids, so this stays cheap.
    /// Inject (or remove) the force-dark stylesheet on an http(s) page, per the current setting.
    fn apply_force_dark(&self, webview: &WebView, url: &str) {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return; // gator:// pages are already themed — don't invert them.
        }
        let on = self.browser.settings.borrow().force_dark;
        let js = if on { FORCE_DARK_JS } else { FORCE_DARK_OFF_JS };
        webview.evaluate_javascript(js.to_string(), |_| {});
    }

    /// Credential Firewall (#19): a password/card field was focused on `origin`/`host`. If the
    /// store is unlocked, there's NO saved login for this exact origin, but there IS one for a
    /// look-alike host (edit distance ≤ 2 — typosquat), warn before the user enters credentials.
    /// Advisory; relies on the navgator:// bridge fetch reaching this intercept (verify on /run).
    fn credential_firewall_check(&self, origin: &str, host: &str) {
        if host.is_empty() {
            return;
        }
        let saved: Vec<String> = {
            let store = self.browser.password_store.borrow();
            if !store.is_unlocked() || !store.for_origin(origin).is_empty() {
                return; // locked, or you legitimately have a login HERE → no warning.
            }
            store.all().iter().map(|c| c.origin.clone()).collect()
        };
        let lookalike = saved
            .iter()
            .filter_map(|o| Url::parse(o).ok().and_then(|u| u.host_str().map(str::to_string)))
            .find(|s| s != host && levenshtein(s, host) <= 2);
        if let Some(s) = lookalike {
            *self.browser.password_msg.borrow_mut() = Some(format!(
                "⚠ You have a saved login for \"{s}\" — this site (\"{host}\") looks similar. \
                 Make sure it's the real one before entering your password."
            ));
            self.window.request_redraw();
        }
    }

    /// Toggle force-dark and apply it to every open tab immediately (and future loads pick it up).
    /// Reader mode: inject the Readability-lite script into the active page (in place; reload exits).
    fn activate_reader_mode(&self) {
        if let Some(tab) = self.active_tab() {
            tab.evaluate_javascript(READER_JS.to_string(), |_| {});
        }
    }

    fn toggle_force_dark(&self) {
        {
            let mut s = self.browser.settings.borrow_mut();
            s.force_dark = !s.force_dark;
            save_settings(&s);
        }
        for pane in 0..2 {
            let tabs = self.pane(pane).tabs.borrow();
            for t in tabs.iter() {
                self.apply_force_dark(&t.webview, &t.url);
            }
        }
        self.window.request_redraw();
    }

    fn apply_cosmetic(&self, pane: usize, tab_idx: usize) {
        if !self.browser.settings.borrow().block_ads {
            return;
        }
        let (webview, url) = {
            let tabs = self.pane(pane).tabs.borrow();
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
            let cosmetic = me.browser.adblock.url_cosmetic_resources(&url);
            let generic = me
                .browser
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
            me.browser.pending_cosmetic.borrow_mut().push((inject, css));
            let _ = me.browser.event_proxy.send_event(WakeUp::CosmeticReady);
        });
    }

    /// Inject queued cosmetic-filter CSS. Called from the event loop (the JS evaluator is free
    /// there, unlike inside an eval callback). Each entry is `(webview, css)`.
    fn flush_cosmetic(&self) {
        let pending = std::mem::take(&mut *self.browser.pending_cosmetic.borrow_mut());
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
            IpcCommand::SelectTab(i) => self.select_tab(self.focused.get(), i),
            IpcCommand::CloseTab(i) => self.close_tab(self.focused.get(), i),
        }
    }

    fn ipc_emit(&self, line: &str) {
        let mut clients = self.browser.ipc_clients.lock().unwrap();
        clients.retain_mut(|c| writeln!(c, "{line}").is_ok());
    }

    /// Forward a mouse/keyboard/wheel input to the active page webview. Returns the engine's
    /// `InputEventId` for the event (used to correlate the page's verdict in
    /// `notify_input_event_handled`), or `None` if there is no active tab to receive it.
    fn forward_to_page(&self, event: InputEvent) -> Option<InputEventId> {
        self.active_tab().map(|tab| tab.notify_input_event(event))
    }

    /// Run an overridable (tier-2) keyboard shortcut — either immediately (when there is no page to
    /// forward the key to, or a dialog/omnibar already owns it) or from `notify_input_event_handled`
    /// once the page has declined to consume the key. See [`KeyShortcut`].
    fn run_key_shortcut(&self, shortcut: KeyShortcut) {
        match shortcut {
            KeyShortcut::CommandPalette => {
                *self.location.borrow_mut() = ">".to_string();
                self.location_dirty.set(true);
                self.focus_omnibox.set(true);
            }
            KeyShortcut::Bookmark => self.toggle_bookmark_active(),
            KeyShortcut::Find => {
                self.find_open.set(true);
                self.find_focus.set(true);
            }
            KeyShortcut::History => {
                if let (Ok(url), Some(tab)) = (Url::parse("gator://history"), self.active_tab()) {
                    self.location_dirty.set(false);
                    tab.load(url);
                }
            }
            KeyShortcut::Downloads => {
                if let (Ok(url), Some(tab)) = (Url::parse("gator://downloads"), self.active_tab()) {
                    self.location_dirty.set(false);
                    tab.load(url);
                }
            }
            KeyShortcut::DevTools => {
                self.show_console.set(!self.show_console.get());
                self.console_filter_focus.set(self.show_console.get());
            }
            KeyShortcut::ZoomIn => self.zoom_in(),
            KeyShortcut::ZoomOut => self.zoom_out(),
            KeyShortcut::ZoomReset => self.zoom_reset(),
            KeyShortcut::ToggleSplit => self.toggle_split(),
        }
        self.window.request_redraw();
    }

    /// Device-px y at which the page area begins (below the chrome).
    fn toolbar_dev(&self) -> f64 {
        self.toolbar_height.get() as f64 * self.scale.get()
    }

    /// Device-px x at which the page area begins (right of the left vertical-tabs panel; 0
    /// in horizontal mode). Mirrors `toolbar_dev` for the x axis.
    fn content_left_dev(&self) -> f64 {
        self.content_left.get() as f64 * self.scale.get()
    }

    /// Device-px width of the right-hand Studio panel (0 when closed); the page ends this
    /// many device px before the window's right edge.
    fn content_right_dev(&self) -> f64 {
        self.content_right.get() as f64 * self.scale.get()
    }

    /// Whether the pointer (last known position) is over a page area — below the toolbar, right of
    /// the vertical-tabs panel, left of the Studio panel. Used to decide whether the page's CSS
    /// cursor (vs the egui chrome's own) should be shown.
    fn over_page_area(&self) -> bool {
        let (cx, cy) = self.cursor.get();
        let win_w = self.window.inner_size().width as f64;
        cy >= self.toolbar_dev()
            && cx >= self.content_left_dev()
            && cx <= win_w - self.content_right_dev()
    }
}

impl WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    /// The page has finished processing an input event we forwarded. For overridable (tier-2)
    /// keyboard shortcuts, this is the verdict: run our shortcut only if the page did NOT consume
    /// the key — i.e. it neither called `preventDefault` nor let Servo's own `<input>` handler eat
    /// it. Mirrors how Chrome runs a browser accelerator only for unhandled key events.
    fn notify_input_event_handled(
        &self,
        _webview: WebView,
        event_id: InputEventId,
        result: InputEventResult,
    ) {
        if let Some(shortcut) = self.pending_shortcuts.borrow_mut().remove(&event_id) {
            if !result
                .intersects(InputEventResult::DefaultPrevented | InputEventResult::Consumed)
            {
                self.run_key_shortcut(shortcut);
            }
        }
    }

    /// The page changed the CSS cursor under the pointer (e.g. a link → pointer, text → I-beam).
    /// Store it for the move handler to keep applying, and — since swervo only emits this while the
    /// pointer is over a page — apply it now too (handles a cursor change under a stationary
    /// pointer, e.g. a hover state or JS that swaps the element).
    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        let icon = map_cursor(cursor);
        self.page_cursor.set(icon);
        if self.over_page_area() {
            self.window.set_cursor(icon);
        }
    }

    /// Surface page `console.*` output and uncaught JS exceptions to the terminal — Servo routes
    /// them here, and the default is a no-op (silently dropped). Gated behind `NAVGATOR_CONSOLE=1`
    /// so normal runs stay quiet; invaluable for diagnosing why a real site renders wrong (e.g. a
    /// JS bundle that throws early, leaving scroll-reveal content stuck at `opacity:0`).
    fn show_console_message(&self, _webview: WebView, level: ConsoleLogLevel, message: String) {
        if std::env::var_os("NAVGATOR_CONSOLE").is_some() {
            eprintln!("[page console {level:?}] {message}");
        }
        // Buffer for the in-app DevTools console (Ctrl+Shift+J), capped so a chatty page can't
        // grow it unbounded.
        let mut buf = self.console_log.borrow_mut();
        buf.push_back(ConsoleMessage { level, text: message });
        while buf.len() > 500 {
            buf.pop_front();
        }
        drop(buf);
        if self.show_console.get() {
            self.window.request_redraw();
        }
    }

    /// Serve NavGator's internal `gator://` pages (e.g. `gator://welcome`). Servo asks the
    /// embedder to intercept every resource load *before* it resolves the scheme, so a custom
    /// scheme works here with no engine fork patch and no net-internal ProtocolHandler. Loads
    /// we don't recognise are left alone (dropping `load` signals "do not intercept").
    fn load_web_resource(&self, webview: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        // GM_* capability bridge (design §5): the injected shim issues
        // `navgator://gm/<cap>/<call>` fetches. Intercept BEFORE adblock so the hot path for
        // ordinary http(s)/gator loads is one extra cheap scheme compare. The <cap> token
        // identifies the calling add-on and is validated against its `granted` set.
        if url.scheme() == "navgator" && url.host_str() == Some("gm") {
            let body = self.handle_gm_bridge(&url);
            let mut headers = HeaderMap::new();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            let response = WebResourceResponse::new(url)
                .status_code(StatusCode::OK)
                .headers(headers);
            let mut intercepted = load.intercept(response);
            intercepted.send_body_data(body);
            intercepted.finish();
            return;
        }
        // Credential Firewall (#19): the injected FIREWALL_JS pings here when a password/card
        // field is focused, so native can warn on a look-alike origin. Reply is an empty {}.
        if url.scheme() == "navgator" && url.host_str() == Some("credfw") {
            let (mut origin, mut host) = (String::new(), String::new());
            for (k, v) in url.query_pairs() {
                match k.as_ref() {
                    "o" => origin = v.into_owned(),
                    "h" => host = v.into_owned(),
                    _ => {}
                }
            }
            self.credential_firewall_check(&origin, &host);
            let response = WebResourceResponse::new(url).status_code(StatusCode::OK);
            let mut intercepted = load.intercept(response);
            intercepted.send_body_data(b"{}".to_vec());
            intercepted.finish();
            return;
        }
        // Record/replay archive (deterministic rendering-regression fixtures). When enabled, every
        // http(s) load is served from / captured to the on-disk archive instead of the live
        // network — bypassing adblock so a fixture is complete and reproducible. See `archive`.
        if let Some(arc) = self.archive.as_ref() {
            if matches!(url.scheme(), "http" | "https") {
                let method = load.request().method.as_str().to_owned();
                let user_agent = load
                    .request()
                    .headers
                    .get(USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let stored = match arc.mode() {
                    archive::Mode::Replay => arc.lookup(&method, url.as_str()),
                    archive::Mode::Record => arc.capture(&method, url.as_str(), user_agent.as_deref()),
                };
                match stored {
                    Some(s) => {
                        let mut headers = HeaderMap::new();
                        for (k, v) in &s.headers {
                            if let (Ok(name), Ok(val)) =
                                (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
                            {
                                headers.insert(name, val);
                            }
                        }
                        let response = WebResourceResponse::new(url.clone())
                            .status_code(StatusCode::from_u16(s.status).unwrap_or(StatusCode::OK))
                            .headers(headers);
                        let mut intercepted = load.intercept(response);
                        if !s.body.is_empty() {
                            intercepted.send_body_data(s.body);
                        }
                        intercepted.finish();
                    },
                    None => {
                        if arc.mode() == archive::Mode::Replay {
                            arc.note_miss(&method, url.as_str());
                        }
                        // Stay offline: fail the load rather than fall through to the network.
                        let response = WebResourceResponse::new(url.clone())
                            .status_code(StatusCode::GATEWAY_TIMEOUT);
                        load.intercept(response).finish();
                    },
                }
                return;
            }
        }
        if url.scheme() != "gator" {
            // Ad/tracker blocking. This delegate already intercepts every load, so a matched
            // request is intercepted with an empty 204 instead of being fetched. `source` is the
            // requesting tab's URL, so first-vs-third-party rules resolve correctly.
            if matches!(url.scheme(), "http" | "https") && self.browser.settings.borrow().block_ads {
                let loc = self.locate_tab(&webview);
                let source = loc
                    .and_then(|(p, i)| self.pane(p).tabs.borrow().get(i).map(|t| t.url.clone()))
                    .unwrap_or_default();
                if let Ok(req) = adblock::request::Request::new(url.as_str(), &source, "other") {
                    if self.browser.adblock.check_network_request(&req).matched {
                        self.browser.adblock_blocked.set(self.browser.adblock_blocked.get() + 1);
                        // Record for the gator://why receipt (per-page, capped).
                        if let Some((p, i)) = loc {
                            if let Some(t) = self.pane(p).tabs.borrow().get(i) {
                                let mut log = t.blocked.borrow_mut();
                                if log.len() < 500 {
                                    log.push(url.as_str().to_string());
                                }
                            }
                        }
                        let response =
                            WebResourceResponse::new(url).status_code(StatusCode::NO_CONTENT);
                        let intercepted = load.intercept(response);
                        intercepted.finish();
                        return;
                    }
                }
            }
            return;
        }
        let body = match url.host_str().unwrap_or("welcome") {
            "welcome" | "newtab" | "home" => self.render_gator_welcome(),
            "why" => self.render_gator_why(),
            "export" => {
                if url.query_pairs().any(|(k, v)| k == "get" && v == "all") {
                    self.export_json()
                } else {
                    self.render_gator_export()
                }
            }
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
            "extensions" | "addons" => {
                // Command links: ?enable=<id> / ?disable=<id> / ?remove=<id>. query_pairs() is
                // already percent-decoded. Apply at most one (each rendered link carries one).
                let mut enable: Option<String> = None;
                let mut disable: Option<String> = None;
                let mut remove: Option<String> = None;
                for (k, v) in url.query_pairs() {
                    match k.as_ref() {
                        "enable" => enable = Some(v.into_owned()),
                        "disable" => disable = Some(v.into_owned()),
                        "remove" => remove = Some(v.into_owned()),
                        _ => {}
                    }
                }
                if let Some(id) = enable {
                    self.set_addon_consent(&userscripts::AddonId(id), true);
                } else if let Some(id) = disable {
                    self.set_addon_consent(&userscripts::AddonId(id), false);
                } else if let Some(id) = remove {
                    self.remove_addon(&userscripts::AddonId(id));
                }
                self.render_gator_extensions()
            }
            "settings" => {
                // Parse the query into one SettingsApply. query_pairs() already percent-decodes,
                // so ?accent=%23ff7a45 arrives as '#ff7a45' and a <form> GET encodes itself. Every
                // rendered link carries exactly one recognized param, so the last-wins fold is fine.
                let mut apply = SettingsApply::None;
                for (k, v) in url.query_pairs() {
                    apply = match k.as_ref() {
                        "engine" => v
                            .parse()
                            .ok()
                            .map(SettingsApply::Engine)
                            .unwrap_or(SettingsApply::None),
                        "search" => SettingsApply::Search(v.into_owned()),
                        "theme" => v
                            .parse()
                            .ok()
                            .map(SettingsApply::Theme)
                            .unwrap_or(SettingsApply::None),
                        "accent" => SettingsApply::Accent(v.into_owned()),
                        "dark" => SettingsApply::Dark(v == "on"),
                        "block_ads" => SettingsApply::BlockAds(v == "on"),
                        "sync_bookmarks" => SettingsApply::SyncBookmarks(v == "on"),
                        "sync_history" => SettingsApply::SyncHistory(v == "on"),
                        "sync_passwords" => SettingsApply::SyncPasswords(v == "on"),
                        "action" => SettingsApply::Action(v.into_owned()),
                        "base" | "accentk" | "density" | "font" | "tabpos" | "tabfit"
                        | "wallpaper" | "preset" | "radius" | "glass" | "tabmaxw" | "module" => {
                            SettingsApply::ThemeSet(k.into_owned(), v.into_owned())
                        }
                        _ => apply,
                    };
                }
                self.render_gator_settings(apply)
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
            // Bundled chrome fonts, served so the HTML new-tab page matches the native chrome
            // (gator://font/grotesk | /outfit | /mono). Same scheme as the page ⇒ same-origin.
            "font" => match url.path().trim_start_matches('/') {
                "outfit" => {
                    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Outfit.ttf"))
                        .to_vec()
                }
                "mono" => include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/fonts/JetBrainsMono.ttf"
                ))
                .to_vec(),
                _ => include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/fonts/SpaceGrotesk.ttf"
                ))
                .to_vec(),
            },
            // Cached favicon for the new-tab tiles (gator://favicon/<host>). A miss returns an
            // empty body, so the tile's <img onerror> drops it and the letter avatar shows (#14).
            "favicon" => favicon_cache_path(url.path().trim_start_matches('/'))
                .and_then(|p| std::fs::read(p).ok())
                .unwrap_or_default(),
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
        // The export download serves JSON as a file attachment; everything else is a normal page.
        let is_export_dl =
            url.host_str() == Some("export") && url.query_pairs().any(|(k, v)| k == "get" && v == "all");
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static(if is_export_dl {
                "application/json; charset=utf-8"
            } else {
                match url.host_str() {
                    Some("font") => "font/ttf",
                    Some("favicon") => "image/png",
                    _ => "text/html; charset=utf-8",
                }
            }),
        );
        if is_export_dl {
            headers.insert(
                CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"navgator-export.json\""),
            );
        }
        let response = WebResourceResponse::new(url)
            .status_code(StatusCode::OK)
            .headers(headers);
        let mut intercepted = load.intercept(response);
        intercepted.send_body_data(body);
        intercepted.finish();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            self.pane(p).tabs.borrow_mut()[i].url = url.to_string();
            // New page → reset its gator://why block receipt (it accumulates as resources load).
            if matches!(url.scheme(), "http" | "https") {
                self.pane(p).tabs.borrow()[i].blocked.borrow_mut().clear();
            }
            let focused = p == self.focused.get();
            if focused {
                self.ipc_emit(&format!("url {i} {url}"));
                if i == self.pane(p).active.get() && !self.location_dirty.get() {
                    *self.location.borrow_mut() = url.to_string();
                }
            }
            let title = self.pane(p).tabs.borrow()[i].title.clone();
            self.record_visit(url.as_str(), &title);
            self.save_session();
            // Also recompute matching userscripts here: omnibox navigation uses tab.load(), which
            // bypasses request_navigation, so this is the only injection hook for those. (UCM
            // scripts apply on the next load regardless of which hook adds them — Servo caveat.)
            if matches!(url.scheme(), "http" | "https") {
                self.inject_userscripts(&webview, url.as_str());
            }
            self.window.request_redraw();
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            let title = title.unwrap_or_else(|| "New tab".to_string());
            if p == self.focused.get() {
                self.ipc_emit(&format!("title {i} {title}"));
            }
            let url = self.pane(p).tabs.borrow()[i].url.clone();
            self.pane(p).tabs.borrow_mut()[i].title = title.clone();
            self.record_visit(&url, &title);
            self.window.request_redraw();
        }
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<Url>, current: usize) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            let mut tabs = self.pane(p).tabs.borrow_mut();
            tabs[i].can_back = current > 0;
            tabs[i].can_forward = current + 1 < entries.len();
            drop(tabs);
            self.window.request_redraw();
        }
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            let fav = webview.favicon();
            // Cache the favicon to disk keyed by host, so the new-tab tiles render from cache
            // (gator://favicon/<host>) instead of a live network fetch (#14).
            if let Some(f) = &fav {
                let host = self
                    .pane(p)
                    .tabs
                    .borrow()
                    .get(i)
                    .and_then(|t| Url::parse(&t.url).ok())
                    .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()));
                if let (Some(host), Some(png)) = (host, favicon_to_png(f)) {
                    if let Some(path) = favicon_cache_path(&host) {
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(path, png);
                    }
                }
            }
            self.pane(p).tabs.borrow_mut()[i].favicon_pending = fav.map(|f| favicon_color_image(&f));
            self.window.request_redraw();
        }
    }

    /// Track which tabs are producing audio (media-session Playing/Paused). Audible BACKGROUND
    /// tabs are kept un-throttled so their playlist/auto-advance JS (which runs on ≥1s timers when
    /// throttled) keeps up — background audio (YouTube Music / Spotify / Plex) stays smooth.
    fn notify_media_session_event(&self, webview: WebView, event: MediaSessionEvent) {
        let MediaSessionEvent::PlaybackStateChange(state) = event else {
            return;
        };
        let playing = matches!(state, MediaSessionPlaybackState::Playing);
        if let Some((p, i)) = self.locate_tab(&webview) {
            if let Some(t) = self.pane(p).tabs.borrow().get(i) {
                t.audible.set(playing);
                // The active tab is never throttled; a background tab follows its audible state.
                let active = p == self.focused.get() && i == self.pane(p).active.get();
                if !active {
                    t.webview.set_throttled(!playing);
                }
            }
            self.window.request_redraw();
        }
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        // Advertise the page colour-scheme (prefers-color-scheme) at every load phase: the
        // HeadParsed phase sets it before the body cascades, so a theme-aware page renders in its
        // native light/dark scheme from the first paint (a notify after the cascade only relayouts).
        webview.notify_theme_change(self.page_color_scheme());
        if let Some((p, i)) = self.locate_tab(&webview) {
            {
                let mut tabs = self.pane(p).tabs.borrow_mut();
                tabs[i].loading = !matches!(status, LoadStatus::Complete);
                // A new load clears any stale hover/status text and the crashed state
                // (a started load means the pipeline is alive again).
                if !matches!(status, LoadStatus::Complete) {
                    tabs[i].status_text = None;
                    tabs[i].crashed = false;
                }
            }
            if matches!(status, LoadStatus::Complete) {
                self.autofill(p, i);
                self.apply_cosmetic(p, i);
                let force_dark = self.browser.settings.borrow().force_dark;
                if let Some(t) = self.pane(p).tabs.borrow().get(i) {
                    if t.url.starts_with("http://") || t.url.starts_with("https://") {
                        // Credential-firewall sensor (#19).
                        t.webview.evaluate_javascript(FIREWALL_JS.to_string(), |_| {});
                        // DOM-render diagnostic (set NAVGATOR_DOMPROBE=1): reports DOM/layout
                        // stats to the console at load, +3s, +6s so a silent CSR hydration
                        // failure (populated-but-not-painted vs never-mounted) is observable.
                        if std::env::var_os("NAVGATOR_DOMPROBE").is_some() {
                            t.webview.evaluate_javascript(DOM_PROBE_JS.to_string(), |_| {});
                        }
                    }
                    if force_dark {
                        self.apply_force_dark(&t.webview, &t.url);
                    }
                }
            }
            if matches!(status, LoadStatus::Complete)
                && p == self.focused.get()
                && i == self.pane(p).active.get()
            {
                self.location_dirty.set(false);
            }
            self.window.request_redraw();
        }
    }

    fn notify_status_text_changed(&self, webview: WebView, status: Option<String>) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            self.pane(p).tabs.borrow_mut()[i].status_text = status;
            self.window.request_redraw();
        }
    }

    fn notify_download_started(&self, _webview: WebView, url: String, path: String) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        *self.browser.download_toast.borrow_mut() = Some(format!("Downloading {name}…"));
        self.browser.downloads.borrow_mut().push(Download {
            url,
            path,
            done: false,
            success: false,
        });
        self.window.request_redraw();
    }

    fn notify_download_completed(&self, _webview: WebView, path: String, success: bool) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        *self.browser.download_toast.borrow_mut() = Some(if success {
            format!("Saved {name}")
        } else {
            format!("Download failed: {name}")
        });
        if let Some(d) = self
            .browser
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
        let ucm = self.make_tab_ucm();
        // Build into the FOCUSED pane's context — `adopt_tab` pushes into `focused_pane()`, so
        // using pane0's context would bind a popup opened in the right split pane to the wrong FBO.
        let mut builder = request
            .builder(self.focused_pane().context.clone())
            .hidpi_scale_factor(Scale::new(self.scale.get() as f32))
            .delegate(me);
        if let Some(ucm) = &ucm {
            builder = builder.user_content_manager(ucm.clone());
        }
        let webview = builder.build();
        self.adopt_tab(webview, ucm);
    }

    fn notify_closed(&self, webview: WebView) {
        if let Some((p, i)) = self.locate_tab(&webview) {
            self.close_tab(p, i);
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

    fn request_permission(&self, webview: WebView, request: PermissionRequest) {
        let origin = self
            .locate_tab(&webview)
            .and_then(|(p, i)| self.pane(p).tabs.borrow().get(i).and_then(|t| origin_of(&t.url)))
            .unwrap_or_default();
        let feature = format!("{:?}", request.feature());
        // A standing "always" grant answers without prompting again.
        if let Some(&allowed) = self
            .browser
            .permission_grants
            .borrow()
            .get(&(origin.clone(), feature.clone()))
        {
            if allowed {
                request.allow();
            } else {
                request.deny();
            }
            return;
        }
        let who = if origin.is_empty() {
            "This site".to_string()
        } else {
            origin.clone()
        };
        let message = format!("{who} is requesting permission: {feature}");
        self.push_dialog(Dialog::Permission {
            message,
            origin,
            feature,
            handle: Some(request),
        });
    }

    /// A pipeline in this tab's webview panicked. Mark the tab crashed and navigate it to the
    /// internal `gator://crash` recovery page (served by `load_web_resource`), carrying the
    /// crashed URL + panic reason so the page can offer a Reload-back-to-that-URL button.
    /// `tab.load` re-spawns the pipeline, so the tab stays usable.
    fn notify_crashed(&self, webview: WebView, reason: String, _backtrace: Option<String>) {
        let Some((p, i)) = self.locate_tab(&webview) else {
            return;
        };
        let crashed_url = {
            let mut tabs = self.pane(p).tabs.borrow_mut();
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

    fn request_navigation(&self, webview: WebView, navigation_request: NavigationRequest) {
        // `gator://omnibar` is a synthetic action target, not a page: the HTML new-tab page links
        // its search box here so a click focuses the real native omnibar. Cancel the navigation
        // (the new-tab page stays put) and raise the focus flag the next egui frame consumes.
        if navigation_request.url.scheme() == "gator"
            && navigation_request.url.host_str() == Some("omnibar")
        {
            // A submitted query (`?q=…`) from the new-tab search box: resolve it (search or URL)
            // and load it, so the user can type + Enter right in the box. A bare click (no query)
            // just focuses the native omnibar. (Read the query before `deny()` consumes the request.)
            let query = navigation_request
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.into_owned());
            navigation_request.deny();
            match query {
                Some(q) if !q.trim().is_empty() => {
                    let target = {
                        let s = self.browser.settings.borrow();
                        omnibox_target(q.trim(), &s.search)
                    };
                    self.navigate_from_omnibox(&target);
                },
                _ => {
                    self.focus_omnibox.set(true);
                    self.window.request_redraw();
                },
            }
            return;
        }
        // Block web pages from *navigating* to gator:// internal pages. Several of them mutate
        // chrome state from query params (gator://settings?…, gator://passwords?remove=…), so a
        // page-initiated navigation there would be a CSRF on the browser itself. Navigation that
        // originates in a gator:// page (internal links) stays allowed, and the omnibox uses
        // tab.load() — which bypasses this delegate entirely — so user navigation is unaffected.
        if navigation_request.url.scheme() == "gator" {
            let from_web = self
                .locate_tab(&webview)
                .and_then(|(p, i)| self.pane(p).tabs.borrow().get(i).map(|t| t.url.clone()))
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
            if from_web {
                navigation_request.deny();
                return;
            }
        }
        // Per-site userscript injection (design §4 Option A): compute the matching enabled add-ons
        // for the target URL and add their wrapped scripts to this tab's UCM *before* allowing the
        // load, so document-start injection beats page scripts. (Servo applies new UCM scripts on
        // the next load; this hook runs before that load proceeds.)
        if matches!(navigation_request.url.scheme(), "http" | "https") {
            let url = navigation_request.url.as_str().to_string();
            self.inject_userscripts(&webview, &url);
        }
        navigation_request.allow();
    }
}

/// Create a brand-new OS window with its own render contexts, egui, and tab set (opening
/// `urls`, or the welcome page if empty). One `Servo` (in `browser`) drives every window — this
/// just registers another rendering context with it when the window's first webview is built.
fn open_window(
    event_loop: &ActiveEventLoop,
    browser: &Rc<BrowserState>,
    urls: Vec<Url>,
) -> (WindowId, Rc<AppState>) {
    let display_handle = event_loop.display_handle().expect("no display handle");
    let window = event_loop
        .create_window(
            Window::default_attributes()
                .with_title("NavGator")
                .with_decorations(false)
                .with_transparent(true)
                .with_visible(false)
                .with_inner_size(LogicalSize::new(1280.0, 800.0)),
        )
        .expect("failed to create window");

    // First window only: if the user hasn't explicitly chosen a theme base, follow the OS
    // colour-scheme — dark OS → dark chrome. Not persisted, so it re-tracks the OS each launch
    // until the user picks a base in the Studio (which sets `theme_base_explicit`).
    if !browser.os_theme_applied.replace(true) {
        let follow_os = !browser.settings.borrow().theme_base_explicit;
        if follow_os && os_prefers_dark(&window) {
            let mut s = browser.settings.borrow_mut();
            if s.theme.base.is_light() {
                s.theme.base = theme::Base::Obsidian;
                sync_legacy_theme(&mut s);
            }
        }
    }

    let window_id = window.id();
    let window_handle = window.window_handle().expect("no window handle");
    let inner = window.inner_size();
    let scale = window.scale_factor();

    // Every window after the first shares the first window's surfman `Connection` (via
    // `new_shared`) so they all render against ONE EGL display — closing a window then can't
    // terminate the display the others use. The first window's context becomes the render seed,
    // held for the app's lifetime so that shared display outlives every window.
    let window_context = Rc::new(
        match browser.render_seed.borrow().as_ref() {
            Some(seed) => seed
                .new_shared(window_handle, inner)
                .expect("failed to create shared WindowRenderingContext"),
            None => WindowRenderingContext::new(display_handle, window_handle, inner)
                .expect("failed to create WindowRenderingContext"),
        },
    );
    if browser.render_seed.borrow().is_none() {
        *browser.render_seed.borrow_mut() = Some(window_context.clone());
    }
    let _ = window_context.make_current();
    let content_context = Rc::new(window_context.offscreen_context(inner));
    // Second offscreen FBO for the right split pane (its own webview + history).
    let content_context_r = Rc::new(window_context.offscreen_context(inner));

    let _ = content_context.make_current();
    let egui = EguiGlow::new(event_loop, content_context.glow_gl_api(), None, None, false);
    egui.egui_ctx.options_mut(|o| {
        o.zoom_with_keyboard = false;
    });
    // Bundle + register the chrome typefaces (Space Grotesk / Outfit / JetBrains Mono).
    fonts::install_fonts(&egui.egui_ctx);
    window.set_visible(true);

    let state = Rc::new(AppState {
        browser: browser.clone(),
        window_context,
        content_context: content_context.clone(),
        egui: RefCell::new(egui),
        corner_mask: RefCell::new(None),
        toolbar_height: Cell::new(0.0),
        content_left: Cell::new(0.0),
        content_right: Cell::new(0.0),
        omni_rect: Cell::new(egui::Rect::ZERO),
        drag_rect: Cell::new(egui::Rect::ZERO),
        thumb_tick: Cell::new(0),
        resize_settle: Cell::new(0),
        pane0: PaneGroup::new(content_context.clone()),
        pane1: PaneGroup::new(content_context_r),
        split: Cell::new(false),
        focused: Cell::new(0),
        location: RefCell::new(String::new()),
        location_dirty: Cell::new(false),
        focus_omnibox: Cell::new(false),
        console_log: RefCell::new(std::collections::VecDeque::new()),
        show_console: Cell::new(false),
        console_filter: RefCell::new(String::new()),
        console_filter_focus: Cell::new(false),
        show_addons: Cell::new(false),
        addon_badge_rect: Cell::new(egui::Rect::NOTHING),
        dialogs: RefCell::new(Vec::new()),
        closed_tabs: RefCell::new(Vec::new()),
        why_log: RefCell::new(Vec::new()),
        find_open: Cell::new(false),
        find_query: RefCell::new(String::new()),
        find_matches: Cell::new(0),
        find_active: Cell::new(0),
        find_focus: Cell::new(false),
        fullscreen: Cell::new(false),
        scale: Cell::new(scale),
        cursor: Cell::new((0.0, 0.0)),
        page_cursor: Cell::new(CursorIcon::Default),
        ctrl: Cell::new(false),
        shift: Cell::new(false),
        alt: Cell::new(false),
        pending_shortcuts: RefCell::new(HashMap::new()),
        weak_self: RefCell::new(Weak::new()),
        archive: archive::ResourceArchive::from_env(),
        wants_close: Cell::new(false),
        window,
    });
    *state.weak_self.borrow_mut() = Rc::downgrade(&state);

    if urls.is_empty() {
        state.new_tab(content_url());
    } else {
        for url in urls {
            state.new_tab(url);
        }
    }
    (window_id, state)
}

enum App {
    Initial {
        waker: Waker,
        ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    },
    Running {
        browser: Rc<BrowserState>,
        windows: HashMap<WindowId, Rc<AppState>>,
    },
}

/// Free a window's webviews (the heavy per-window Servo resources). Each webview also holds a
/// strong `Rc<AppState>` as its delegate (AppState *is* the `WebViewDelegate`), so clearing the
/// tabs additionally breaks that reference cycle.
fn drop_window_webviews(state: &AppState) {
    state.pane0.tabs.borrow_mut().clear();
    state.pane1.tabs.borrow_mut().clear();
}

/// Retire a closed window: free its webviews, hide the OS window, and drop its `AppState` (which
/// tears down just this window's `WindowRenderingContext` — its surfman context + surface).
///
/// This used to `mem::forget` the shell to avoid a crash: every window built its own surfman
/// `Connection`, and since surfman hands back the *same* `EGLDisplay` for the same X11 display
/// while each `Connection` independently owns it, the first window to drop `eglTerminate`d the
/// display the others were still rendering with (`MakeCurrentFailed(BadDisplay)` panic). Now every
/// window after the first is built with `WindowRenderingContext::new_shared`, so they all share one
/// `Connection`; the display outlives them all (kept alive by the render seed in `BrowserState`),
/// and a normal drop is safe.
fn retire_window(state: Rc<AppState>) {
    drop_window_webviews(&state);
    state.window.set_visible(false);
    drop(state);
}

impl ApplicationHandler<WakeUp> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (waker, ipc_clients) = match self {
            App::Initial { waker, ipc_clients } => (waker.clone(), ipc_clients.clone()),
            App::Running { .. } => return,
        };
        let event_proxy = waker.0.clone();

        // Build the single Servo engine (window-agnostic — it drives every window's webviews via
        // one Painter per rendering context) and the browser-global services, ONCE.
        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker))
            .preferences(navgator_preferences())
            .opts({
                let mut opts = Opts {
                    // Process-isolate page content — a crash or exploit in a content process can't
                    // take down the chrome (the security half of the pitch). Set
                    // `NAVGATOR_SINGLE_PROCESS=1` to run single-process for diagnostics (so page JS
                    // console errors / panics land in the main log instead of a separate content proc).
                    multiprocess: std::env::var_os("NAVGATOR_SINGLE_PROCESS").is_none(),
                    // OS-confine each content process (gaol: user namespace + chroot + seccomp).
                    // Opt-in (see sandbox_enabled): gaol panics unrecoverably where unprivileged
                    // namespaces are denied, which sysctls don't reliably predict.
                    sandbox: sandbox_enabled(),
                    ..Default::default()
                };
                // Point the engine at our profile dir so its net-layer state persists across
                // restarts: cookies (logins survive a restart), HSTS, HTTP auth, and the HTTP
                // cache (LYK-1382 — cold starts reuse cached bodies instead of re-downloading).
                // Without this the engine keeps all of that in memory only. Private/incognito
                // webviews use a separate in-memory state and are unaffected.
                if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| std::path::PathBuf::from(h).join(".config"))
                    })
                    .map(|base| base.join("navgator"))
                {
                    let _ = std::fs::create_dir_all(&dir);
                    opts.config_dir = Some(dir);
                }
                // Engine diagnostics dumps, e.g. `NAVGATOR_DEBUG=scroll-tree,display-list`
                // (see servo_config DiagnosticsLoggingOption for the full list). Combine with
                // NAVGATOR_SINGLE_PROCESS=1 so the dumps land in this process's log.
                if let Ok(d) = std::env::var("NAVGATOR_DEBUG") {
                    if let Err(e) = opts.debug.extend_from_string(&d) {
                        eprintln!("NAVGATOR_DEBUG: {e}");
                    }
                }
                opts
            })
            .build();
        servo.setup_logging();

        // Load the add-on registry: scan the userscripts dir, parse Greasemonkey metadata,
        // reconcile with the persisted consent state in addons.json. New/changed scripts default
        // to disabled-pending-consent. Per-tab injection (not a single shared UCM) selects the
        // matching enabled add-ons per navigation (see `inject_userscripts`).
        let addons = load_addons();
        let userscripts_count = addons.addons.len();

        let (sync_cb, sync_ch, sync_cp) = load_sync_cursors();
        let browser = Rc::new(BrowserState {
            servo,
            render_seed: RefCell::new(None),
            addons: RefCell::new(addons),
            gm_salt: password::random_salt(),
            userscripts_count,
            ipc_clients,
            settings: RefCell::new(load_settings()),
            os_theme_applied: Cell::new(false),
            profile: RefCell::new(load_profile()),
            sync_cursor_bookmarks: Cell::new(sync_cb),
            sync_cursor_history: Cell::new(sync_ch),
            sync_cursor_passwords: Cell::new(sync_cp),
            adblock: adblock::Engine::from_rules(
                load_filter_rules(),
                adblock::lists::ParseOptions::default(),
            ),
            adblock_blocked: Cell::new(0),
            permission_grants: RefCell::new(load_permission_grants()),
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
            import_msg: RefCell::new(None),
            event_proxy,
            pending_windows: RefCell::new(Vec::new()),
        });

        // Restore the previous session's tabs unless the user passed an explicit URL on the
        // command line. A missing/empty session => the welcome page (handled by open_window).
        let (restored, restored_pane1) = if cli_url_given() {
            (Vec::new(), Vec::new())
        } else {
            load_session()
        };
        let (window_id, state) = open_window(event_loop, &browser, restored);
        // Re-enter split if the saved session had a second pane.
        if !restored_pane1.is_empty() {
            state.restore_split(restored_pane1);
        }

        // Auto-unlock the password store from the OS keyring if the user opted in (browser-global).
        // Fully fallible-safe: `fetch()` returns None on any keyring problem (no secret-service,
        // headless, NoEntry), so this never blocks or crashes startup.
        let remember = browser.settings.borrow().remember_passphrase;
        if remember && let Some(pass) = keyring_store::fetch() {
            state.unlock_passwords(&pass);
        }

        // Surface an install-consent prompt for any userscript that's installed but not yet
        // decided (new/changed scripts default to disabled-pending-consent in load_addons).
        state.prompt_pending_consents();

        let mut windows = HashMap::new();
        windows.insert(window_id, state);
        *self = App::Running { browser, windows };
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WakeUp) {
        let App::Running { browser, windows } = self else { return };
        if let WakeUp::Exit = event {
            // Gracefully shut the engine down first so its network thread flushes cookies, HSTS,
            // auth, and the HTTP cache to disk (we otherwise leak the Servo handles at exit, so
            // Servo's own Drop-based shutdown never runs). See Servo::shutdown / LYK-1382.
            browser.servo.shutdown();
            event_loop.exit();
            return;
        }
        // These wake-ups act on browser-global data; route through any one window's state, then
        // refresh all windows so chrome (bookmarks bar, sync status) reflects the change.
        if let Some(state) = windows.values().next().cloned() {
            match event {
                WakeUp::Ipc(cmd) => state.handle_ipc(cmd),
                WakeUp::SyncDone(outcome) => state.apply_sync(outcome),
                WakeUp::CosmeticReady => state.flush_cosmetic(),
                WakeUp::Wake | WakeUp::Exit => {}
            }
        }
        // The change above is browser-global (bookmarks bar, sync status, …); refresh EVERY
        // window's chrome, not just the one we routed it through, so none show stale state.
        for st in windows.values() {
            st.window.request_redraw();
        }
        browser.servo.spin_event_loop();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let App::Running { browser, windows } = self else { return };
        let browser = browser.clone();
        browser.servo.spin_event_loop();

        // Closing a window drops just that window's state (its Drop frees its GL contexts); the
        // last window closing quits the app.
        if matches!(event, WindowEvent::CloseRequested) {
            if let Some(state) = windows.remove(&id) {
                retire_window(state);
            }
            if windows.is_empty() {
                browser.servo.shutdown();
                event_loop.exit();
            } else {
                for w in windows.values() {
                    w.window.request_redraw();
                }
            }
            return;
        }
        let Some(state) = windows.get(&id).cloned() else {
            return;
        };

        // Window-level events handled before egui.
        match &event {
            WindowEvent::RedrawRequested => {
                // Close this window if requested (✕ button or its last tab closed); quit only if
                // it was the last window.
                if state.wants_close.get() {
                    // Retire this window (frees its webviews + hides it, leaking the shared GL
                    // shell so the other windows' EGL display survives — see retire_window). Quit
                    // only when the last *visible* window is gone.
                    if let Some(st) = windows.remove(&id) {
                        retire_window(st);
                    }
                    if windows.is_empty() {
                        browser.servo.shutdown();
                        event_loop.exit();
                    } else {
                        // Repaint the survivors so they don't sit stale after a sibling closed.
                        for w in windows.values() {
                            w.window.request_redraw();
                        }
                    }
                    return;
                }
                state.update();
                state.paint();
                // Open any windows requested during the frame (Ctrl+N / tab pop-out). Done here,
                // outside the per-window borrow, since this is where we own the `windows` map.
                let pending: Vec<Url> = browser.pending_windows.borrow_mut().drain(..).collect();
                for url in pending {
                    let (wid, ws) = open_window(event_loop, &browser, vec![url]);
                    windows.insert(wid, ws);
                }
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
                state.alt.set(m.state().alt_key());
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
        let left_dev = state.content_left_dev();
        let right_dev = state.content_right_dev();
        let win_w = state.window.inner_size().width as f64;
        // Device-px x of the split divider. Round in LOGICAL space then scale so it lands exactly
        // on the visual divider (`mid = ((avail.left()+avail.right())*0.5).round()` in update()),
        // rather than the unrounded device-px midpoint — otherwise the input/visual seam can
        // disagree by up to ~scale/2 px (#9).
        let mid_dev = ((left_dev + win_w - right_dev) / (2.0 * scale)).round() * scale;
        let (cx, cy) = state.cursor.get();
        let over_toolbar = cy < toolbar_dev;
        // The page area is below the toolbar, right of the vertical-tabs panel, and left of
        // the Studio panel. Input over any chrome region must not leak to the page. (The new-tab
        // page is now a real HTML document, so it receives input like any other page.)
        let over_chrome = cy < toolbar_dev || cx < left_dev || cx > win_w - right_dev;
        let dialog_open = !state.dialogs.borrow().is_empty();

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                match state.resize_direction_at(position.x, position.y) {
                    Some(dir) => state.window.set_cursor(resize_cursor(dir)),
                    // Over a page, keep showing the page's CSS cursor (swervo only re-emits it when
                    // the hovered element's cursor changes, so moving within one element must not
                    // reset it to the default arrow). Over the chrome, egui owns the cursor.
                    None if !over_chrome => state.window.set_cursor(state.page_cursor.get()),
                    // Over the chrome, let egui own the cursor: it sets the correct per-widget icon
                    // each frame (a text I-beam over the omnibar, a pointer over buttons/dropdowns).
                    // Forcing Default here raced egui and produced the wrong cursor.
                    None => {}
                }
                let foc = state.focused.get();
                let off = if state.split.get() && foc == 1 { mid_dev } else { left_dev };
                let over_focused = !state.split.get() || ((foc == 0) == (cx < mid_dev));
                if !(resp.consumed || over_chrome || dialog_open) && over_focused {
                    state.forward_to_page(InputEvent::MouseMove(MouseMoveEvent::new(
                        DevicePoint::new(
                            (position.x - off) as f32,
                            (position.y - toolbar_dev) as f32,
                        )
                        .into(),
                    )));
                }
            }

            WindowEvent::MouseInput {
                state: bs,
                button,
                device_id,
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
                    && !over_chrome
                    && !dialog_open
                {
                    state.push_dialog(Dialog::ContextMenu {
                        pos: egui::pos2((cx / scale) as f32, (cy / scale) as f32),
                    });
                    return;
                }
                // Drag the window ONLY from the reserved drag handle (left of the window controls).
                // Nothing else is draggable — not the omnibar, not other toolbar space, not widgets.
                let over_drag = state
                    .drag_rect
                    .get()
                    .contains(egui::pos2((cx / scale) as f32, (cy / scale) as f32));
                if button == MouseButton::Left
                    && bs == ElementState::Pressed
                    && over_drag
                    && !resp.consumed
                {
                    let _ = state.window.drag_window();
                    // The OS window-drag swallows the button-release, so egui never sees the pointer
                    // go up: it was already fed the *press* (line ~7824) and, with no matching
                    // release, stays "pressed" — which suppresses hover cursors (egui only shows a
                    // hover/on_hover_cursor icon when no button is down) until the next real click.
                    // Resetting `input.pointer` isn't enough: the press is buffered in egui-winit and
                    // re-applied next frame. Feed egui a synthetic release (same device/button) so the
                    // buffered press balances out and the per-widget cursors recover after the drag.
                    let release = WindowEvent::MouseInput {
                        device_id,
                        state: ElementState::Released,
                        button: MouseButton::Left,
                    };
                    let _ = state
                        .egui
                        .borrow_mut()
                        .on_window_event(&state.window, &release);
                    state.window.request_redraw();
                    return;
                }
                // Split: a left-press in a pane focuses that pane (so chrome + input follow it).
                if button == MouseButton::Left
                    && bs == ElementState::Pressed
                    && state.split.get()
                    && !over_chrome
                    && !over_toolbar
                {
                    state.focused.set(if cx < mid_dev { 0 } else { 1 });
                }
                let foc = state.focused.get();
                let off = if state.split.get() && foc == 1 { mid_dev } else { left_dev };
                let over_focused = !state.split.get() || ((foc == 0) == (cx < mid_dev));
                if !(resp.consumed || over_chrome || dialog_open) && over_focused {
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
                        DevicePoint::new((cx - off) as f32, (cy - toolbar_dev) as f32).into(),
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
                let foc = state.focused.get();
                let off = if state.split.get() && foc == 1 { mid_dev } else { left_dev };
                let over_focused = !state.split.get() || ((foc == 0) == (cx < mid_dev));
                if !(resp.consumed || over_chrome || dialog_open) && over_focused {
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
                        DevicePoint::new((cx - off) as f32, (cy - toolbar_dev) as f32).into(),
                    )));
                }
            }

            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                // ── Keyboard model (mirrors Chrome's two tiers) ─────────────────────────────────
                let pressed = matches!(key_event.state, ElementState::Pressed);
                // Modifiers forwarded to the page so its own keydown handlers + text-editing
                // shortcuts see the real chord (Ctrl+A/C/X/V — LYK-1309 — and site shortcuts).
                let modifiers = {
                    let mut m = Modifiers::empty();
                    if state.ctrl.get() {
                        m |= Modifiers::CONTROL;
                    }
                    if state.shift.get() {
                        m |= Modifiers::SHIFT;
                    }
                    if state.alt.get() {
                        m |= Modifiers::ALT;
                    }
                    m
                };

                // A plain Ctrl / Ctrl+Shift chord (never with Alt — adding Alt makes it a different,
                // non-reserved combo that belongs to the page, e.g. crossdraw.app's Ctrl+Shift+Alt+N).
                if pressed && state.ctrl.get() && !state.alt.get() {
                    // Tier 1 — RESERVED shortcuts: tab/window lifecycle, tab switching, quit, reload
                    // and the omnibox escape hatch. Handled here and NEVER delivered to the page, so a
                    // hostile or buggy page can't trap the user (block closing a tab, opening a window,
                    // switching away, quitting).
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
                            state.close_tab(state.focused.get(), state.focused_pane().active.get());
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("q") => {
                            // Quit the whole app (every window). The event loop exits on this.
                            let _ = state.browser.event_proxy.send_event(WakeUp::Exit);
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("l") => {
                            state.focus_omnibox.set(true);
                            state.window.request_redraw();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("n") => {
                            // New OS window (queued; opened by the event loop on the next redraw).
                            state
                                .browser
                                .pending_windows
                                .borrow_mut()
                                .push(content_url());
                            state.window.request_redraw();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("r") => {
                            if let Some(tab) = state.active_tab() {
                                tab.reload();
                            }
                            return;
                        }
                        // Ctrl+1..9 select a tab (Ctrl+0 is zoom-reset, a tier-2 shortcut below).
                        WinitKey::Character(c)
                            if c.parse::<usize>().map(|n| (1..=9).contains(&n)).unwrap_or(false) =>
                        {
                            let n = c.parse::<usize>().unwrap_or(0);
                            let len = state.focused_pane().tabs.borrow().len();
                            if n == 9 && len > 0 {
                                state.select_tab(state.focused.get(), len - 1);
                            } else if (1..=8).contains(&n) && n <= len {
                                state.select_tab(state.focused.get(), n - 1);
                            }
                            return;
                        }
                        WinitKey::Named(NamedKey::Tab) => {
                            let len = state.focused_pane().tabs.borrow().len();
                            if len > 1 {
                                let cur = state.focused_pane().active.get();
                                let next = if state.shift.get() {
                                    (cur + len - 1) % len
                                } else {
                                    (cur + 1) % len
                                };
                                state.select_tab(state.focused.get(), next);
                            }
                            return;
                        }
                        _ => {}
                    }

                    // Tier 2 — OVERRIDABLE shortcuts: palette, bookmark, find, history, downloads,
                    // devtools, zoom, split. Forward the key to the page FIRST and run ours only if
                    // the page doesn't consume it (see `notify_input_event_handled`). This lets web
                    // apps own Ctrl+F etc. exactly like Chrome.
                    let fallback = match &key_event.logical_key {
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("k") => {
                            Some(KeyShortcut::CommandPalette)
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("d") => {
                            Some(KeyShortcut::Bookmark)
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("f") => {
                            Some(KeyShortcut::Find)
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("h") => {
                            Some(KeyShortcut::History)
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("j") => {
                            Some(if state.shift.get() {
                                KeyShortcut::DevTools
                            } else {
                                KeyShortcut::Downloads
                            })
                        }
                        WinitKey::Character(c) if c == "=" || c == "+" => Some(KeyShortcut::ZoomIn),
                        WinitKey::Character(c) if c == "-" || c == "_" => Some(KeyShortcut::ZoomOut),
                        WinitKey::Character(c) if c == "0" => Some(KeyShortcut::ZoomReset),
                        WinitKey::Character(c) if c == "\\" => Some(KeyShortcut::ToggleSplit),
                        _ => None,
                    };
                    if let Some(shortcut) = fallback {
                        // Forward to the page unless egui already owns the key (omnibar/find focused),
                        // a dialog is up, or there is no page — in those cases run ours immediately.
                        let forwarded = if resp.consumed || dialog_open {
                            None
                        } else {
                            winit_key_to_servo(&key_event.logical_key).and_then(|key| {
                                let mut ke = KeyboardEvent::from_state_and_key(KeyState::Down, key);
                                ke.event.modifiers = modifiers;
                                state.forward_to_page(InputEvent::Keyboard(ke))
                            })
                        };
                        match forwarded {
                            Some(id) => {
                                let mut pend = state.pending_shortcuts.borrow_mut();
                                // Defensive bound: if verdicts ever stop arriving (tab torn down
                                // mid-flight), don't let the map grow without limit.
                                if pend.len() > 128 {
                                    pend.clear();
                                }
                                pend.insert(id, shortcut);
                            }
                            None => state.run_key_shortcut(shortcut),
                        }
                        return;
                    }
                }
                // F5 reloads the active tab (Ctrl+R is reserved above; F5 carries no Ctrl).
                if pressed && matches!(key_event.logical_key, WinitKey::Named(NamedKey::F5)) {
                    if let Some(tab) = state.active_tab() {
                        tab.reload();
                    }
                    return;
                }
                // Esc closes find, else a context menu, else exits page fullscreen.
                if pressed && matches!(key_event.logical_key, WinitKey::Named(NamedKey::Escape)) {
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
                // Everything else → the page (both key-down and key-up).
                if !(resp.consumed || dialog_open) {
                    if let Some(key) = winit_key_to_servo(&key_event.logical_key) {
                        let key_state = match key_event.state {
                            ElementState::Pressed => KeyState::Down,
                            ElementState::Released => KeyState::Up,
                        };
                        let mut keyboard_event = KeyboardEvent::from_state_and_key(key_state, key);
                        keyboard_event.event.modifiers = modifiers;
                        state.forward_to_page(InputEvent::Keyboard(keyboard_event));
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

    #[test]
    fn parses_chrome_bookmark_tree() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{ "roots": { "bookmark_bar": { "type":"folder", "children": [
                {"type":"url","name":"Example","url":"https://example.com"},
                {"type":"folder","name":"Sub","children":[
                    {"type":"url","name":"Inner","url":"https://inner.test/page"},
                    {"type":"url","name":"Internal","url":"chrome://settings"}
                ]}
            ]}}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        for node in json.get("roots").unwrap().as_object().unwrap().values() {
            super::collect_chrome_bookmarks(node, &mut out);
        }
        // two http(s) bookmarks recursed out of the tree; chrome:// filtered
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|(u, n)| u == "https://example.com" && n == "Example"));
        assert!(out.iter().any(|(u, _)| u == "https://inner.test/page"));
    }
}

#[cfg(test)]
mod chrome_helper_tests {
    use super::{
        color_hex, favicon_hue, is_hex_color, levenshtein, omnibox_target, parse_session,
        strip_tracking_params,
    };

    #[test]
    fn levenshtein_flags_typosquats() {
        assert_eq!(levenshtein("github.com", "github.com"), 0);
        assert_eq!(levenshtein("github.com", "gihub.com"), 1); // dropped a char
        assert_eq!(levenshtein("paypal.com", "paypa1.com"), 1); // l -> 1
        assert_eq!(levenshtein("google.com", "gooogle.com"), 1); // extra o
        // A real firewall trigger (≤ 2) vs clearly-different / subdomain-appended (not).
        assert!(levenshtein("paypal.com", "paypal.com") <= 2);
        assert!(levenshtein("apple.com", "microsoft.com") > 2);
        assert!(levenshtein("bank.com", "bank.com.evil.com") > 2);
    }

    #[test]
    fn strip_tracking_params_removes_trackers() {
        // Trackers dropped, real params kept, order preserved.
        assert_eq!(
            strip_tracking_params("https://x.com/p?utm_source=a&id=5&fbclid=zz"),
            "https://x.com/p?id=5"
        );
        // All-tracker query collapses to no query (no dangling '?').
        assert_eq!(
            strip_tracking_params("https://x.com/p?utm_campaign=a&gclid=b"),
            "https://x.com/p"
        );
        // Untouched when there's nothing to strip / it doesn't parse.
        assert_eq!(strip_tracking_params("https://x.com/p?id=5"), "https://x.com/p?id=5");
        assert_eq!(strip_tracking_params("https://x.com/p"), "https://x.com/p");
        assert_eq!(strip_tracking_params("not a url"), "not a url");
        // Omnibar URL entries are cleaned; searches are left alone.
        assert_eq!(
            omnibox_target("https://x.com/p?utm_medium=x&q=2", "https://d.com/?q=%s"),
            "https://x.com/p?q=2"
        );
        assert!(omnibox_target("hello utm_source", "https://d.com/?q=%s").starts_with("https://d.com/?q="));
    }

    #[test]
    fn omnibox_target_classifies_and_blocks_javascript() {
        let s = "https://duck.com/?q=%s";
        // A real URL passes through unchanged.
        assert_eq!(omnibox_target("https://example.com", s), "https://example.com");
        // A bare domain is promoted to https.
        assert_eq!(omnibox_target("example.com", s), "https://example.com");
        // Free text becomes a search.
        assert!(omnibox_target("hello world", s).starts_with("https://duck.com/?q="));
        // javascript: is ALWAYS routed to search, never loaded — including the `://` variant and
        // mixed case / leading whitespace.
        for js in [
            "javascript:alert(1)",
            "javascript://%0aalert(1)",
            "  JavaScript:alert(document.domain)",
        ] {
            assert!(
                omnibox_target(js, s).starts_with("https://duck.com/?q="),
                "{js} should be searched, not loaded"
            );
        }
    }

    #[test]
    fn is_hex_color_rejects_injection() {
        assert!(is_hex_color("#fff"));
        assert!(is_hex_color("#5b8cff"));
        assert!(is_hex_color("#ABC123"));
        // The XSS vector this guards: a value that breaks out of the gator:// page <style>.
        assert!(!is_hex_color("#fff}</style><script>alert(1)</script>"));
        assert!(!is_hex_color("#fffe")); // 4 hex digits
        assert!(!is_hex_color("#xyz")); // non-hex
        assert!(!is_hex_color("red")); // no leading #
        assert!(!is_hex_color("#"));
        assert!(!is_hex_color(""));
    }

    #[test]
    fn parse_session_flat_and_split() {
        // Flat (non-split) session: every URL lands in pane 0.
        let (p0, p1) = parse_session("https://a.com\nhttps://b.com\n");
        assert_eq!(p0.len(), 2);
        assert!(p1.is_empty());

        // Split session: the #PANE1 marker routes the rest to pane 1.
        let (p0, p1) = parse_session("https://a.com\nhttps://b.com\n#PANE1\nhttps://c.com\n");
        assert_eq!(
            p0.iter().map(|u| u.as_str()).collect::<Vec<_>>(),
            ["https://a.com/", "https://b.com/"]
        );
        assert_eq!(
            p1.iter().map(|u| u.as_str()).collect::<Vec<_>>(),
            ["https://c.com/"]
        );

        // Blank + non-URL lines are skipped (forward/back compatible).
        let (p0, p1) = parse_session("\nnot a url\nhttps://a.com\n\n");
        assert_eq!(p0.len(), 1);
        assert!(p1.is_empty());
    }

    #[test]
    fn favicon_hue_is_stable_and_in_range() {
        for s in ["figma.com", "github.com", "", "a very long tab title here"] {
            let h = favicon_hue(s);
            assert!((0.0..360.0).contains(&h), "{s}: {h}");
            assert_eq!(h, favicon_hue(s), "must be deterministic");
        }
        // distinct strings should (here) give distinct hues
        assert_ne!(favicon_hue("github.com"), favicon_hue("figma.com"));
    }

    #[test]
    fn color_hex_formats_rrggbb() {
        assert_eq!(color_hex(egui::Color32::from_rgb(0x5b, 0x8c, 0xff)), "#5b8cff");
        assert_eq!(color_hex(egui::Color32::BLACK), "#000000");
        assert_eq!(color_hex(egui::Color32::from_rgb(255, 255, 255)), "#ffffff");
    }
}

impl AppState {
    fn render_console_panel(&self, ctx: &egui::Context) {
        if !self.show_console.get() {
            return;
        }
        let pal = self.browser.settings.borrow().theme.palette();
        let screen = ctx.content_rect();
        let height = (screen.height() * 0.32).clamp(140.0, 320.0);
        let frame = egui::Frame::NONE
            .fill(pal.bg2)
            .stroke(egui::Stroke::new(1.0, pal.border))
            .inner_margin(egui::Margin::same(6));
        egui::Area::new(egui::Id::new("devtools_console"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(screen.width());
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Console").strong().color(pal.text));
                        let count = self.console_log.borrow().len();
                        ui.label(egui::RichText::new(format!("{count}")).small().weak());
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("filter").small().weak());
                        let mut filter = self.console_filter.borrow_mut();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut *filter)
                                .desired_width(180.0)
                                .hint_text("level or text"),
                        );
                        if self.console_filter_focus.take() {
                            resp.request_focus();
                        }
                        drop(filter);
                        if ui.button("Clear").clicked() {
                            self.console_log.borrow_mut().clear();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if icon_button(ui, true, "Close", &pal, icon::close).clicked() {
                                self.show_console.set(false);
                            }
                        });
                    });
                    ui.separator();
                    let filter = self.console_filter.borrow().to_lowercase();
                    egui::ScrollArea::vertical()
                        .max_height(height)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_width(screen.width() - 16.0);
                            for msg in self.console_log.borrow().iter() {
                                let level = format!("{:?}", msg.level).to_lowercase();
                                if !filter.is_empty()
                                    && !level.contains(&filter)
                                    && !msg.text.to_lowercase().contains(&filter)
                                {
                                    continue;
                                }
                                let color = match msg.level {
                                    ConsoleLogLevel::Error => egui::Color32::from_rgb(0xf0, 0x6b, 0x6b),
                                    ConsoleLogLevel::Warn => egui::Color32::from_rgb(0xe6, 0xb4, 0x50),
                                    ConsoleLogLevel::Info => pal.accent,
                                    _ => pal.text,
                                };
                                ui.label(
                                    egui::RichText::new(&msg.text)
                                        .monospace()
                                        .size(11.5)
                                        .color(color),
                                );
                            }
                        });
                });
            });
    }
}

