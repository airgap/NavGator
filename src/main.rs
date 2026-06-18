//! swerve — a web browser whose UI ("chrome") is HTML rendered by Servo.
//!
//! ## Milestone 4 (this file): tabs, + dynamic content rect (M3 polish)
//! Builds on the M2 compositor and M3 bridge.
//!
//! * **Tabs** — the content area is a `Vec<Tab>` of webviews that all share the one
//!   `OffscreenRenderingContext`; only the active tab is shown and painted. The
//!   engine pushes a tab model (`{tabs:[{title}], active, url, canGoBack/Forward}`)
//!   to the chrome via the `swerve:state` event, and the chrome renders the tab
//!   strip from it. Tab actions come back as `swerve:tab?new|select=i|close=i`.
//! * **Dynamic content rect (retires fixed `CHROME_HEIGHT`)** — on load/resize the
//!   chrome reports its content region's top (CSS px) via `swerve:ready?top=` /
//!   `swerve:layout?top=`; the engine derives the content rect from that, so the
//!   chrome/engine split is whatever the chrome actually lays out.
//!
//! A `Weak<AppState>` self-reference lets `&self` delegate callbacks build new tab
//! webviews (which need the `Rc<AppState>` as their delegate).
//!
//! API verified against servo rev `ed1af70`. Bridge/compositing: see M2/M3 notes.
//! TODO: IME/composition; popup/prompt/context-menu hooks; a less hacky command
//! channel than `swerve:` navigation.

use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::thread;

use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
use servo::{
    DevicePoint, InputEvent, Key, KeyState, KeyboardEvent, NamedKey as ServoNamedKey,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NavigationRequest, OffscreenRenderingContext, RenderingContext, Servo, ServoBuilder, WebView,
    WebViewBuilder, WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

/// Fallback chrome height (logical px) used until the chrome reports its real
/// content-region top via the `swerve:ready`/`swerve:layout` bridge command.
const CHROME_HEIGHT_FALLBACK: u32 = 84;

fn main() -> Result<(), Box<dyn Error>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let event_loop = EventLoop::with_user_event().build()?;

    // Optional IPC control socket (M5): when SWERVE_IPC is set, an external process
    // can drive the engine over it. `ipc_clients` holds connected clients' write
    // halves so the UI thread can push state events to them.
    let ipc_clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    if let Ok(path) = env::var("SWERVE_IPC") {
        start_ipc(path, event_loop.create_proxy(), ipc_clients.clone());
    }

    let mut app = App::Initial {
        waker: Waker(event_loop.create_proxy()),
        ipc_clients,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn file_url(rel: &str) -> Url {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    Url::from_file_path(&p).unwrap_or_else(|_| Url::parse("about:blank").unwrap())
}

fn chrome_url() -> Url {
    file_url("src/chrome/index.html")
}

fn content_url() -> Url {
    if let Some(arg) = env::args().nth(1) {
        if let Ok(url) = Url::parse(&arg) {
            return url;
        }
        eprintln!("swerve: '{arg}' is not a valid URL, loading the home page instead");
    }
    file_url("src/content/home.html")
}

fn content_size(window: PhysicalSize<u32>, top: u32) -> PhysicalSize<u32> {
    PhysicalSize::new(
        window.width.max(1),
        window.height.saturating_sub(top).max(1),
    )
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
}

struct AppState {
    window: Window,
    servo: Servo,
    window_context: Rc<WindowRenderingContext>,
    content_context: Rc<OffscreenRenderingContext>,
    chrome: RefCell<Option<WebView>>,
    tabs: RefCell<Vec<Tab>>,
    active: Cell<usize>,
    /// Device-px y where the content region starts (reported by the chrome).
    content_top: Cell<u32>,
    scale: Cell<f64>,
    cursor: Cell<(f64, f64)>,
    focused: Cell<Focused>,
    /// Whether a Ctrl modifier is currently held (for tab shortcuts).
    ctrl: Cell<bool>,
    /// Self-reference so `&self` delegate callbacks can build webviews (which need
    /// the `Rc<AppState>` as their delegate).
    weak_self: RefCell<Weak<AppState>>,
    /// Connected IPC clients' write halves, for pushing state events.
    ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
}

impl AppState {
    fn content_phys_size(&self) -> PhysicalSize<u32> {
        content_size(self.window.inner_size(), self.content_top.get())
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

        if let Some(blit) = self.content_context.render_to_parent_callback() {
            let win = self.window.inner_size();
            let w = win.width.max(1) as i32;
            let h = win.height.saturating_sub(self.content_top.get()).max(1) as i32;
            let target = Rect::new(Point2D::new(0, 0), Size2D::new(w, h));
            let gl = self.window_context.glow_gl_api();
            blit(&*gl, target);
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
        webview.resize(self.content_phys_size());
        let idx = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(Tab {
                webview,
                url: String::new(),
                title: "New tab".to_string(),
                can_back: false,
                can_forward: false,
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
        if self.tabs.borrow().is_empty() {
            self.new_tab(content_url());
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
                j.push_str(&format!("{{title:{}}}", js_string(&t.title)));
            }
            j.push(']');
            let (url, cb, cf) = tabs
                .get(active)
                .map(|t| (t.url.clone(), t.can_back, t.can_forward))
                .unwrap_or_default();
            (j, active, url, cb, cf)
        };
        self.chrome_eval(format!(
            "window.dispatchEvent(new CustomEvent('swerve:state',{{detail:{{tabs:{tabs_json},active:{active},url:{},canGoBack:{can_back},canGoForward:{can_forward}}}}}))",
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
            "ready" => {
                self.apply_layout(url);
                self.push_model();
            }
            "layout" => self.apply_layout(url),
            other => eprintln!("swerve: unknown chrome command '{other}'"),
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
    fn apply_layout(&self, url: &Url) {
        for (key, value) in url.query_pairs() {
            if key == "top" {
                if let Ok(css_top) = value.parse::<f64>() {
                    let dev = (css_top * self.scale.get()).round().max(0.0) as u32;
                    if dev != self.content_top.get() && dev < self.window.inner_size().height {
                        self.content_top.set(dev);
                        let csize = self.content_phys_size();
                        self.content_context.resize(csize);
                        for tab in self.tabs.borrow().iter() {
                            tab.webview.resize(csize);
                        }
                        self.window.request_redraw();
                    }
                }
            }
        }
    }
}

impl servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].url = url.to_string();
            self.ipc_emit(&format!("url {i} {url}"));
            self.push_model();
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

    fn request_navigation(&self, webview: WebView, navigation_request: NavigationRequest) {
        // Chrome navigations to the `swerve:` scheme are commands, not real loads.
        if self.is_chrome(&webview) && navigation_request.url.scheme() == "swerve" {
            let url = navigation_request.url.clone();
            navigation_request.deny();
            self.handle_chrome_command(&url);
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

        let display_handle = event_loop.display_handle().expect("no display handle");
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title("swerve")
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
            scale: Cell::new(scale),
            cursor: Cell::new((0.0, 0.0)),
            focused: Cell::new(Focused::Content),
            ctrl: Cell::new(false),
            weak_self: RefCell::new(Weak::new()),
            ipc_clients,
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: WakeUp) {
        if let App::Running(state) = self {
            if let WakeUp::Ipc(cmd) = event {
                state.handle_ipc(cmd);
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
                        WinitKey::Named(NamedKey::Tab) => {
                            let len = state.tabs.borrow().len();
                            if len > 1 {
                                state.select_tab((state.active.get() + 1) % len);
                            }
                            return;
                        }
                        _ => {}
                    }
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
}

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let _ = self.0.send_event(WakeUp::Wake);
    }
}

/// A command from an external process over the IPC control socket — the seed of
/// the "Servo as an external engine" goal (M5): other apps drive the engine.
#[derive(Debug)]
enum IpcCommand {
    Navigate(String),
    NewTab,
    Reload,
    Back,
    Forward,
    SelectTab(usize),
    CloseTab(usize),
}

impl IpcCommand {
    /// Parse one line of the text protocol, e.g. `navigate https://servo.org`.
    fn parse(line: &str) -> Option<Self> {
        let mut parts = line.trim().splitn(2, ' ');
        let verb = parts.next()?;
        let arg = parts.next().unwrap_or("").trim();
        Some(match verb {
            "navigate" => IpcCommand::Navigate(arg.to_string()),
            "new-tab" => IpcCommand::NewTab,
            "reload" => IpcCommand::Reload,
            "back" => IpcCommand::Back,
            "forward" => IpcCommand::Forward,
            "select-tab" => IpcCommand::SelectTab(arg.parse().ok()?),
            "close-tab" => IpcCommand::CloseTab(arg.parse().ok()?),
            _ => return None,
        })
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
            eprintln!("swerve: could not bind IPC socket {path}: {e}");
            return;
        }
    };
    eprintln!("swerve: IPC control socket listening on {path}");
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
