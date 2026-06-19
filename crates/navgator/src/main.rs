//! navgator — a web browser whose UI ("chrome") is HTML rendered by Servo.
//!
//! ## Milestone 4 (this file): tabs, + dynamic content rect (M3 polish)
//! Builds on the M2 compositor and M3 bridge.
//!
//! * **Tabs** — the content area is a `Vec<Tab>` of webviews that all share the one
//!   `OffscreenRenderingContext`; only the active tab is shown and painted. The
//!   engine pushes a tab model (`{tabs:[{title}], active, url, canGoBack/Forward}`)
//!   to the chrome via the `navgator:state` event, and the chrome renders the tab
//!   strip from it. Tab actions come back as `navgator:tab?new|select=i|close=i`.
//! * **Dynamic content rect (retires fixed `CHROME_HEIGHT`)** — on load/resize the
//!   chrome reports its content region's top (CSS px) via `navgator:ready?top=` /
//!   `navgator:layout?top=`; the engine derives the content rect from that, so the
//!   chrome/engine split is whatever the chrome actually lays out.
//!
//! A `Weak<AppState>` self-reference lets `&self` delegate callbacks build new tab
//! webviews (which need the `Rc<AppState>` as their delegate).
//!
//! API verified against servo rev `ed1af70`. Bridge/compositing: see M2/M3 notes.
//! TODO: IME/composition; popup/prompt/context-menu hooks; a less hacky command
//! channel than `navgator:` navigation.

use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::thread;

use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
// Everything from the engine comes through navgator-engine, the only crate that
// touches the Servo fork (ROADMAP §R2; docs/FORK.md). The IPC wire types come from
// the servo-free navgator-protocol crate.
use navgator_engine::{
    CreateNewWebViewRequest, DevicePoint, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent,
    LoadStatus, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey as ServoNamedKey, NavigationRequest, OffscreenRenderingContext, RenderingContext,
    Servo, ServoBuilder, WebView, WebViewBuilder, WebViewDelegate, WheelDelta, WheelEvent,
    WheelMode, WindowRenderingContext, EmbedderControl, EmbedderControlId, SimpleDialog,
};
use navgator_protocol::IpcCommand;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowId};

/// Fallback chrome height (logical px) used until the chrome reports its real
/// content-region top via the `navgator:ready`/`navgator:layout` bridge command.
const CHROME_HEIGHT_FALLBACK: u32 = 84;

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

    // Optional IPC control socket (M5): when NAVGATOR_IPC is set, an external process
    // can drive the engine over it. `ipc_clients` holds connected clients' write
    // halves so the UI thread can push state events to them.
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

/// Resolve navgator's bundled web assets. A packaged build keeps them next to the
/// executable in `<exe_dir>/resources/{chrome,content}`; `cargo run` (no such dir)
/// falls back to the source tree. Without this, `env!("CARGO_MANIFEST_DIR")` would
/// point at the build machine's path and a distributed binary couldn't find its UI.
fn resources_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let res = dir.join("resources");
            if res.join("chrome/index.html").exists() {
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

fn chrome_url() -> Url {
    file_url("chrome/index.html")
}

fn content_url() -> Url {
    if let Some(arg) = env::args().nth(1) {
        if let Ok(url) = Url::parse(&arg) {
            return url;
        }
        eprintln!("navgator: '{arg}' is not a valid URL, loading the home page instead");
    }
    file_url("content/home.html")
}

/// User settings, persisted to a small `key=value` config file.
#[derive(Clone)]
struct Settings {
    /// Search URL template; `%s` is replaced with the URL-encoded query.
    search: String,
    /// Chrome accent color (any CSS color).
    accent: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search: "https://duckduckgo.com/?q=%s".to_string(),
            accent: "#5b8cff".to_string(),
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
        let _ = std::fs::write(&path, format!("search={}\naccent={}\n", s.search, s.accent));
    }
}

fn settings_url() -> Url {
    file_url("content/settings.html")
}

fn content_size(window: PhysicalSize<u32>, top: u32) -> PhysicalSize<u32> {
    PhysicalSize::new(
        window.width.max(1),
        window.height.saturating_sub(top).max(1),
    )
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

#[derive(Clone, Copy, PartialEq)]
enum Focused {
    Chrome,
    Content,
}

/// One browser tab: a content webview plus the state we mirror into the chrome.
struct Tab {
    webview: WebView,
    url: String,
    title: String,
    can_back: bool,
    can_forward: bool,
    zoom: f32,
    loading: bool,
}

struct AppState {
    window: Window,
    servo: Servo,
    window_context: Rc<WindowRenderingContext>,
    content_context: Rc<OffscreenRenderingContext>,
    chrome: RefCell<Option<WebView>>,
    tabs: RefCell<Vec<Tab>>,
    active: Cell<usize>,
    /// Device-px y where the content region starts (effective; forced to 0 in fullscreen).
    content_top: Cell<u32>,
    /// The chrome's last reported content top; restored when leaving fullscreen.
    chrome_top: Cell<u32>,
    /// Whether the active page is in fullscreen (chrome hidden, content fills the window).
    fullscreen: Cell<bool>,
    /// Whether a chrome overlay (modal dialog) is active; the content blit is skipped so
    /// the chrome — which renders the whole window — shows the modal over the page region.
    overlay: Cell<bool>,
    /// The JS dialog awaiting a user response, if any.
    pending_dialog: RefCell<Option<SimpleDialog>>,
    scale: Cell<f64>,
    cursor: Cell<(f64, f64)>,
    focused: Cell<Focused>,
    /// Whether a Ctrl modifier is currently held (for tab shortcuts).
    ctrl: Cell<bool>,
    /// Whether a Shift modifier is currently held (Ctrl+Shift+Tab, etc.).
    shift: Cell<bool>,
    /// Self-reference so `&self` delegate callbacks can build webviews (which need
    /// the `Rc<AppState>` as their delegate).
    weak_self: RefCell<Weak<AppState>>,
    /// Connected IPC clients' write halves, for pushing state events.
    ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    /// User settings (search engine, accent), persisted to disk.
    settings: RefCell<Settings>,
    /// Proxy onto the winit loop, used to request app exit from `&self` callbacks.
    event_proxy: EventLoopProxy<WakeUp>,
}

impl AppState {
    fn content_phys_size(&self) -> PhysicalSize<u32> {
        content_size(self.window.inner_size(), self.content_top.get())
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

    fn active_tab(&self) -> Option<WebView> {
        self.tabs.borrow().get(self.active.get()).map(|t| t.webview.clone())
    }

    fn tab_index(&self, webview: &WebView) -> Option<usize> {
        self.tabs.borrow().iter().position(|t| &t.webview == webview)
    }

    fn render(&self) {
        let chrome = self.chrome.borrow();
        let (Some(chrome), Some(active)) = (chrome.as_ref(), self.active_tab()) else {
            return;
        };

        let _ = self.content_context.make_current();
        active.paint();

        let _ = self.window_context.make_current();
        self.window_context.prepare_for_rendering();
        chrome.paint();

        // While a chrome overlay (modal) is up, skip the content blit so the chrome's
        // full-window render (with the modal) shows over the page region.
        if !self.overlay.get() {
            if let Some(blit) = self.content_context.render_to_parent_callback() {
                let win = self.window.inner_size();
                let w = win.width.max(1) as i32;
                let h = win.height.saturating_sub(self.content_top.get()).max(1) as i32;
                let target = Rect::new(Point2D::new(0, 0), Size2D::new(w, h));
                let gl = self.window_context.glow_gl_api();
                blit(&*gl, target);
            }
        }

        self.window_context.present();
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        self.window_context.resize(size);
        if let Some(chrome) = self.chrome.borrow().as_ref() {
            chrome.resize(size);
        }
        let csize = self.content_phys_size();
        self.content_context.resize(csize);
        for tab in self.tabs.borrow().iter() {
            tab.webview.resize(csize);
        }
        self.window.request_redraw();
    }

    fn route(&self, x: f64, y: f64) -> Option<(WebView, DevicePoint)> {
        // A modal overlay is drawn by the chrome across the whole window.
        if self.overlay.get() {
            return Some((self.chrome.borrow().clone()?, DevicePoint::new(x as f32, y as f32)));
        }
        let top = self.content_top.get() as f64;
        if y < top {
            Some((self.chrome.borrow().clone()?, DevicePoint::new(x as f32, y as f32)))
        } else {
            Some((self.active_tab()?, DevicePoint::new(x as f32, (y - top) as f32)))
        }
    }

    fn focused_webview(&self) -> Option<WebView> {
        match self.focused.get() {
            Focused::Chrome => self.chrome.borrow().clone(),
            Focused::Content => self.active_tab(),
        }
    }

    fn is_chrome(&self, webview: &WebView) -> bool {
        self.chrome.borrow().as_ref() == Some(webview)
    }

    fn chrome_eval(&self, js: String) {
        if let Some(chrome) = self.chrome.borrow().as_ref() {
            chrome.evaluate_javascript(js, |_| {});
        }
    }

    /// Focus + select the address bar (Ctrl+L).
    fn focus_omnibox(&self) {
        self.focused.set(Focused::Chrome);
        self.chrome_eval(
            "var a=document.getElementById('address');if(a){a.focus();a.select();}".to_string(),
        );
    }

    // ── Page zoom (Ctrl +/-/0, Ctrl+wheel) ────────────────────────────────────
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

    // ── Modal overlay (JS dialogs) ─────────────────────────────────────────────
    fn show_dialog(&self, dialog: SimpleDialog) {
        let (kind, default) = match &dialog {
            SimpleDialog::Alert(_) => ("alert", String::new()),
            SimpleDialog::Confirm(_) => ("confirm", String::new()),
            SimpleDialog::Prompt(p) => ("prompt", p.current_value().to_string()),
        };
        let message = dialog.message().to_string();
        *self.pending_dialog.borrow_mut() = Some(dialog);
        self.overlay.set(true);
        self.focused.set(Focused::Chrome);
        self.chrome_eval(format!(
            "window.dispatchEvent(new CustomEvent('navgator:dialog',{{detail:{{kind:{},message:{},value:{}}}}}))",
            js_string(kind),
            js_string(&message),
            js_string(&default),
        ));
        self.window.request_redraw();
    }

    fn resolve_dialog(&self, url: &Url) {
        let dialog = self.pending_dialog.borrow_mut().take();
        self.overlay.set(false);
        self.focused.set(Focused::Content);
        let Some(dialog) = dialog else {
            self.window.request_redraw();
            return;
        };
        let mut ok = false;
        let mut value = String::new();
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "action" => ok = v == "ok",
                "value" => value = v.to_string(),
                _ => {}
            }
        }
        if ok {
            match dialog {
                SimpleDialog::Prompt(mut p) => {
                    p.set_current_value(&value);
                    p.confirm();
                }
                other => other.confirm(),
            }
        } else {
            dialog.dismiss();
        }
        self.window.request_redraw();
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

    /// Resize a freshly-built content webview, push it as a tab, and focus it.
    fn adopt_tab(&self, webview: WebView) {
        webview.resize(self.content_phys_size());
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
        }
        self.active.set(i);
        self.focused.set(Focused::Content);
        self.push_model();
        self.window.request_redraw();
    }

    fn close_tab(&self, i: usize) {
        {
            let mut tabs = self.tabs.borrow_mut();
            if i >= tabs.len() {
                return;
            }
            tabs.remove(i); // dropping the WebView handle closes the webview
        }
        // Closing the last tab closes the window (request exit on the winit loop).
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

    /// Push the full tab model to the chrome UI.
    fn push_model(&self) {
        let (tabs_json, active, url, can_back, can_forward) = {
            let tabs = self.tabs.borrow();
            let active = self.active.get();
            let mut j = String::from("[");
            for (i, t) in tabs.iter().enumerate() {
                if i > 0 {
                    j.push(',');
                }
                j.push_str(&format!(
                    "{{title:{},loading:{}}}",
                    js_string(&t.title),
                    t.loading
                ));
            }
            j.push(']');
            let (url, cb, cf) = tabs
                .get(active)
                .map(|t| (t.url.clone(), t.can_back, t.can_forward))
                .unwrap_or_default();
            (j, active, url, cb, cf)
        };
        self.chrome_eval(format!(
            "window.dispatchEvent(new CustomEvent('navgator:state',{{detail:{{tabs:{tabs_json},active:{active},url:{},canGoBack:{can_back},canGoForward:{can_forward}}}}}))",
            js_string(&url)
        ));
    }

    // ── Bridge command handling ───────────────────────────────────────────────
    fn handle_chrome_command(&self, url: &Url) {
        match url.path() {
            "nav" => {
                if let (Some(target), Some(tab)) = (
                    url.fragment().and_then(|f| Url::parse(f).ok()),
                    self.active_tab(),
                ) {
                    tab.load(target);
                }
            }
            "back" => {
                if let Some(tab) = self.active_tab() {
                    tab.go_back(1);
                }
            }
            "forward" => {
                if let Some(tab) = self.active_tab() {
                    tab.go_forward(1);
                }
            }
            "reload" => {
                if let Some(tab) = self.active_tab() {
                    tab.reload();
                }
            }
            "tab" => self.handle_tab_command(url),
            "window" => self.handle_window_command(url),
            "settings" => self.new_tab(settings_url()),
            "dialog" => self.resolve_dialog(url),
            "ready" => {
                self.apply_layout(url);
                self.push_model();
                self.apply_settings_to_chrome();
            }
            "layout" => self.apply_layout(url),
            other => eprintln!("navgator: unknown chrome command '{other}'"),
        }
    }

    fn handle_tab_command(&self, url: &Url) {
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "new" => self.new_tab(content_url()),
                "select" => {
                    if let Ok(i) = value.parse::<usize>() {
                        self.select_tab(i);
                    }
                }
                "close" => {
                    if let Ok(i) = value.parse::<usize>() {
                        self.close_tab(i);
                    }
                }
                _ => {}
            }
        }
    }

    /// Window controls — we draw our own, the OS titlebar is disabled.
    fn handle_window_command(&self, url: &Url) {
        for (key, value) in url.query_pairs() {
            if key == "action" {
                match value.as_ref() {
                    "minimize" => self.window.set_minimized(true),
                    "maximize" => self.window.set_maximized(!self.window.is_maximized()),
                    "close" => {
                        let _ = self.event_proxy.send_event(WakeUp::Exit);
                    }
                    "drag" => {
                        let _ = self.window.drag_window();
                    }
                    _ => {}
                }
            }
        }
    }

    fn is_settings_page(&self, webview: &WebView) -> bool {
        self.tab_index(webview)
            .map(|i| self.tabs.borrow()[i].url.ends_with("content/settings.html"))
            .unwrap_or(false)
    }

    /// Settings-page bridge: our trusted settings page can read/write settings.
    fn handle_settings_command(&self, webview: &WebView, url: &Url) {
        match url.path() {
            "settings-get" => self.push_settings_to(webview),
            "settings-set" => {
                {
                    let mut s = self.settings.borrow_mut();
                    for (k, v) in url.query_pairs() {
                        match k.as_ref() {
                            "search" => s.search = v.to_string(),
                            "accent" => s.accent = v.to_string(),
                            _ => {}
                        }
                    }
                    save_settings(&s);
                }
                self.apply_settings_to_chrome();
                self.push_settings_to(webview);
            }
            _ => {}
        }
    }

    fn settings_event_js(&self) -> String {
        let s = self.settings.borrow();
        format!(
            "window.dispatchEvent(new CustomEvent('navgator:settings',{{detail:{{search:{},accent:{}}}}}))",
            js_string(&s.search),
            js_string(&s.accent)
        )
    }

    fn push_settings_to(&self, webview: &WebView) {
        webview.evaluate_javascript(self.settings_event_js(), |_| {});
    }

    fn apply_settings_to_chrome(&self) {
        self.chrome_eval(self.settings_event_js());
    }

    /// Execute a command received over the IPC control socket.
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

    /// Send a line to every connected IPC client, dropping any that error.
    fn ipc_emit(&self, line: &str) {
        let mut clients = self.ipc_clients.lock().unwrap();
        clients.retain_mut(|c| writeln!(c, "{line}").is_ok());
    }

    /// Update the content-region top from a chrome-reported CSS-px value.
    /// Set the effective content-region top and resize the content webviews to match.
    fn set_content_top(&self, dev: u32) {
        if dev == self.content_top.get() {
            return;
        }
        self.content_top.set(dev);
        let csize = self.content_phys_size();
        self.content_context.resize(csize);
        for tab in self.tabs.borrow().iter() {
            tab.webview.resize(csize);
        }
        self.window.request_redraw();
    }

    fn apply_layout(&self, url: &Url) {
        for (key, value) in url.query_pairs() {
            if key == "top" {
                if let Ok(css_top) = value.parse::<f64>() {
                    let dev = (css_top * self.scale.get()).round().max(0.0) as u32;
                    if dev < self.window.inner_size().height {
                        self.chrome_top.set(dev);
                        // In fullscreen the content covers the chrome (top forced to 0);
                        // just remember the chrome's top so we can restore it on exit.
                        if !self.fullscreen.get() {
                            self.set_content_top(dev);
                        }
                    }
                }
            }
        }
    }
}

impl WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].url = url.to_string();
            self.ipc_emit(&format!("url {i} {url}"));
            self.push_model();
            // Hand the settings page its current values once it loads.
            if url.as_str().ends_with("content/settings.html") {
                self.push_settings_to(&webview);
            }
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if let Some(i) = self.tab_index(&webview) {
            let title = title.unwrap_or_else(|| "New tab".to_string());
            self.ipc_emit(&format!("title {i} {title}"));
            self.tabs.borrow_mut()[i].title = title;
            self.push_model();
        }
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<Url>, current: usize) {
        if let Some(i) = self.tab_index(&webview) {
            {
                let mut tabs = self.tabs.borrow_mut();
                tabs[i].can_back = current > 0;
                tabs[i].can_forward = current + 1 < entries.len();
            }
            self.push_model();
        }
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].loading = !matches!(status, LoadStatus::Complete);
            self.push_model();
        }
    }

    fn request_create_new(&self, _parent: WebView, request: CreateNewWebViewRequest) {
        let Some(me) = self.weak_self.borrow().upgrade() else {
            return;
        };
        // window.open / target=_blank → a new foreground tab (sharing the content context).
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
            EmbedderControl::SimpleDialog(dialog) => self.show_dialog(dialog),
            // Context menu / select / file / color pickers / IME: not yet implemented
            // (they need a non-modal overlay or native menus — see docs/ROADMAP).
            _ => {}
        }
    }

    fn hide_embedder_control(&self, _webview: WebView, _control_id: EmbedderControlId) {
        // The page withdrew a control (e.g. navigated away mid-dialog); clear any overlay.
        if self.pending_dialog.borrow().is_some() {
            *self.pending_dialog.borrow_mut() = None;
            self.overlay.set(false);
            self.focused.set(Focused::Content);
            self.window.request_redraw();
        }
    }

    fn notify_fullscreen_state_changed(&self, _webview: WebView, is_fullscreen: bool) {
        self.fullscreen.set(is_fullscreen);
        let top = if is_fullscreen { 0 } else { self.chrome_top.get() };
        self.set_content_top(top);
    }

    fn request_navigation(&self, webview: WebView, navigation_request: NavigationRequest) {
        // `navgator:` navigations are bridge commands, not real loads. Honored from the
        // chrome and from our own (trusted) settings page; ignored from any other web
        // content so a random site can't drive the browser.
        if navigation_request.url.scheme() == "navgator" {
            let url = navigation_request.url.clone();
            navigation_request.deny();
            if self.is_chrome(&webview) {
                self.handle_chrome_command(&url);
            } else if self.is_settings_page(&webview) {
                self.handle_settings_command(&webview, &url);
            }
        } else {
            navigation_request.allow();
        }
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
        // A proxy for requesting app exit from `&self` callbacks (window close, last tab).
        let event_proxy = waker.0.clone();

        let display_handle = event_loop.display_handle().expect("no display handle");
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title("NavGator")
                    .with_decorations(false)
                    .with_inner_size(LogicalSize::new(1280.0, 800.0)),
            )
            .expect("failed to create window");
        let window_handle = window.window_handle().expect("no window handle");

        let inner = window.inner_size();
        let scale = window.scale_factor();
        let content_top = (CHROME_HEIGHT_FALLBACK as f64 * scale).round() as u32;

        let window_context = Rc::new(
            WindowRenderingContext::new(display_handle, window_handle, inner)
                .expect("failed to create WindowRenderingContext"),
        );
        let _ = window_context.make_current();

        let content_context =
            Rc::new(window_context.offscreen_context(content_size(inner, content_top)));

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker))
            .build();
        servo.setup_logging();

        let state = Rc::new(AppState {
            window,
            servo,
            window_context,
            content_context,
            chrome: RefCell::new(None),
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            content_top: Cell::new(content_top),
            chrome_top: Cell::new(content_top),
            fullscreen: Cell::new(false),
            overlay: Cell::new(false),
            pending_dialog: RefCell::new(None),
            scale: Cell::new(scale),
            cursor: Cell::new((0.0, 0.0)),
            focused: Cell::new(Focused::Content),
            ctrl: Cell::new(false),
            shift: Cell::new(false),
            weak_self: RefCell::new(Weak::new()),
            ipc_clients,
            settings: RefCell::new(load_settings()),
            event_proxy,
        });
        *state.weak_self.borrow_mut() = Rc::downgrade(&state);

        let chrome = WebViewBuilder::new(&state.servo, state.window_context.clone())
            .url(chrome_url())
            .hidpi_scale_factor(Scale::new(scale as f32))
            .delegate(state.clone())
            .build();
        *state.chrome.borrow_mut() = Some(chrome);

        // First tab.
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

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => state.render(),
            WindowEvent::Resized(size) => state.resize(size),

            WindowEvent::CursorMoved { position, .. } => {
                state.cursor.set((position.x, position.y));
                match state.resize_direction_at(position.x, position.y) {
                    Some(dir) => state.window.set_cursor(resize_cursor(dir)),
                    None => state.window.set_cursor(CursorIcon::Default),
                }
                if let Some((webview, point)) = state.route(position.x, position.y) {
                    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                        point.into(),
                    )));
                }
            }

            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => {
                let (x, y) = state.cursor.get();
                // Borderless-window edge resize: a left-press in the edge band starts a
                // system resize; don't forward it to a webview.
                if button == MouseButton::Left && matches!(button_state, ElementState::Pressed) {
                    if let Some(dir) = state.resize_direction_at(x, y) {
                        let _ = state.window.drag_resize_window(dir);
                        return;
                    }
                }
                if matches!(button_state, ElementState::Pressed) {
                    let top = state.content_top.get() as f64;
                    state
                        .focused
                        .set(if y < top { Focused::Chrome } else { Focused::Content });
                }
                if let Some((webview, point)) = state.route(x, y) {
                    let action = match button_state {
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
                    if matches!(button_state, ElementState::Pressed) {
                        webview.focus();
                    }
                    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                        action,
                        servo_button,
                        point.into(),
                    )));
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Ctrl+wheel zooms the active tab instead of scrolling.
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
                let (x, y) = state.cursor.get();
                if let Some((webview, point)) = state.route(x, y) {
                    let (dx, dy, mode) = match delta {
                        MouseScrollDelta::LineDelta(lx, ly) => {
                            ((lx * 76.0) as f64, (ly * 76.0) as f64, WheelMode::DeltaLine)
                        }
                        MouseScrollDelta::PixelDelta(p) => (p.x, p.y, WheelMode::DeltaPixel),
                    };
                    let delta = WheelDelta { x: dx, y: dy, z: 0.0, mode };
                    webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                        delta,
                        point.into(),
                    )));
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                state.ctrl.set(modifiers.state().control_key());
                state.shift.set(modifiers.state().shift_key());
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Ctrl-based tab shortcuts are handled here and not forwarded.
                if matches!(key_event.state, ElementState::Pressed) && state.ctrl.get() {
                    match &key_event.logical_key {
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("t") => {
                            state.new_tab(content_url());
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("w") => {
                            state.close_tab(state.active.get());
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("l") => {
                            state.focus_omnibox();
                            return;
                        }
                        WinitKey::Character(c) if c.eq_ignore_ascii_case("r") => {
                            if let Some(tab) = state.active_tab() {
                                tab.reload();
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
                            // Ctrl+1..8 → that tab; Ctrl+9 → last tab.
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
                                // Ctrl+Shift+Tab cycles backward.
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
                // Esc exits page fullscreen (the engine then restores the chrome).
                if state.fullscreen.get()
                    && matches!(key_event.state, ElementState::Pressed)
                    && matches!(key_event.logical_key, WinitKey::Named(NamedKey::Escape))
                {
                    if let Some(tab) = state.active_tab() {
                        tab.exit_fullscreen();
                    }
                    return;
                }
                if let Some(key) = winit_key_to_servo(&key_event.logical_key) {
                    let key_state = match key_event.state {
                        ElementState::Pressed => KeyState::Down,
                        ElementState::Released => KeyState::Up,
                    };
                    if let Some(webview) = state.focused_webview() {
                        webview.notify_input_event(InputEvent::Keyboard(
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
/// Each line read is parsed into an [`IpcCommand`] and posted to the UI loop; the
/// connection's write half is registered to receive state events. Unix-only for now
/// (Windows would use a named pipe).
fn start_ipc(path: String, proxy: EventLoopProxy<WakeUp>, clients: Arc<Mutex<Vec<UnixStream>>>) {
    let _ = std::fs::remove_file(&path); // clear a stale socket
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
