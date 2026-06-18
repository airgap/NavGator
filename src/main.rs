//! swerve — a web browser whose UI ("chrome") is HTML rendered by Servo.
//!
//! ## Milestone 3 (this file): the chrome ↔ engine bridge
//! Builds on M2's two-webview compositing to make swerve an actual browser:
//!
//! * **Keyboard** — winit key events are mapped to Servo `KeyboardEvent`s and routed
//!   to the focused webview (so you can type in the omnibox).
//! * **Chrome → engine** — chrome JS navigates to a `swerve:` command URL
//!   (`swerve:nav#<url>`, `swerve:back`, `swerve:forward`, `swerve:reload`); the
//!   chrome webview's `request_navigation` delegate intercepts it, `deny()`s the
//!   chrome navigation, and drives the content webview. No IPC channel needed —
//!   this is a single process (Verso needed `ipc-channel` only because versoview is
//!   a separate process).
//! * **Engine → chrome** — the content webview's delegate notifications
//!   (`notify_url_changed`, `notify_page_title_changed`, `notify_history_changed`)
//!   are pushed into the chrome via `WebView::evaluate_javascript`, dispatching the
//!   `swerve:state` event that chrome.js listens for (URL bar, tab title, back/fwd).
//!
//! API verified against servo rev `ed1af70`. Compositing/input details: see M2 notes.
//!
//! Still TODO: dynamic content-rect reporting (retire the fixed `CHROME_HEIGHT`),
//! IME/composition, and a less hacky command channel than `swerve:` navigation.

use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
use servo::{
    DevicePoint, InputEvent, Key, KeyState, KeyboardEvent, NamedKey as ServoNamedKey,
    MouseButton as ServoMouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NavigationRequest, OffscreenRenderingContext,
    RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder, WheelDelta, WheelEvent,
    WheelMode, WindowRenderingContext,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

/// Logical-pixel height of the HTML chrome. MUST match `.chrome { height: ... }`
/// in `src/chrome/chrome.css`.
const CHROME_HEIGHT_CSS: u32 = 84;

fn main() -> Result<(), Box<dyn Error>> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let event_loop = EventLoop::with_user_event().build()?;
    let mut app = App::new(&event_loop);
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

fn content_size(window: PhysicalSize<u32>, chrome_h: u32) -> PhysicalSize<u32> {
    PhysicalSize::new(
        window.width.max(1),
        window.height.saturating_sub(chrome_h).max(1),
    )
}

/// Escape a string into a JS double-quoted string literal, for safe interpolation
/// into an `evaluate_javascript` snippet.
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

/// Map a winit logical key to a Servo key. Minimal: printable characters plus the
/// editing/navigation keys needed in the omnibox. (Full coverage would adapt
/// servoshell's `keyutils.rs`.)
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

struct AppState {
    window: Window,
    servo: Servo,
    window_context: Rc<WindowRenderingContext>,
    content_context: Rc<OffscreenRenderingContext>,
    chrome: RefCell<Option<WebView>>,
    content: RefCell<Option<WebView>>,
    chrome_height: Cell<u32>,
    cursor: Cell<(f64, f64)>,
    /// Which webview receives keyboard input.
    focused: Cell<Focused>,
}

impl AppState {
    fn render(&self) {
        let chrome = self.chrome.borrow();
        let content = self.content.borrow();
        let (Some(chrome), Some(content)) = (chrome.as_ref(), content.as_ref()) else {
            return;
        };

        let _ = self.content_context.make_current();
        content.paint();

        let _ = self.window_context.make_current();
        self.window_context.prepare_for_rendering();
        chrome.paint();

        if let Some(blit) = self.content_context.render_to_parent_callback() {
            let win = self.window.inner_size();
            let w = win.width.max(1) as i32;
            let h = win.height.saturating_sub(self.chrome_height.get()).max(1) as i32;
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
        let csize = content_size(size, self.chrome_height.get());
        self.content_context.resize(csize);
        if let Some(content) = self.content.borrow().as_ref() {
            content.resize(csize);
        }
        self.window.request_redraw();
    }

    /// The webview under a window-space point, plus that point translated into the
    /// webview's own coordinate space. Chrome owns `y < chrome_height`.
    fn route(&self, x: f64, y: f64) -> Option<(WebView, DevicePoint)> {
        let chrome_h = self.chrome_height.get() as f64;
        if y < chrome_h {
            let webview = self.chrome.borrow().clone()?;
            Some((webview, DevicePoint::new(x as f32, y as f32)))
        } else {
            let webview = self.content.borrow().clone()?;
            Some((webview, DevicePoint::new(x as f32, (y - chrome_h) as f32)))
        }
    }

    fn focused_webview(&self) -> Option<WebView> {
        match self.focused.get() {
            Focused::Chrome => self.chrome.borrow().clone(),
            Focused::Content => self.content.borrow().clone(),
        }
    }

    fn is_content(&self, webview: &WebView) -> bool {
        self.content.borrow().as_ref() == Some(webview)
    }

    fn is_chrome(&self, webview: &WebView) -> bool {
        self.chrome.borrow().as_ref() == Some(webview)
    }

    /// Run JS in the chrome webview (used to push engine state into the UI).
    fn chrome_eval(&self, js: String) {
        if let Some(chrome) = self.chrome.borrow().as_ref() {
            chrome.evaluate_javascript(js, |_| {});
        }
    }

    /// Act on a `swerve:` command URL emitted by the chrome JS.
    fn handle_chrome_command(&self, url: &Url) {
        let content = self.content.borrow();
        let Some(content) = content.as_ref() else {
            return;
        };
        match url.path() {
            "nav" => {
                if let Some(target) = url.fragment().and_then(|f| Url::parse(f).ok()) {
                    content.load(target);
                }
            }
            "back" => {
                content.go_back(1);
            }
            "forward" => {
                content.go_forward(1);
            }
            "reload" => {
                content.reload();
            }
            other => eprintln!("swerve: unknown chrome command '{other}'"),
        }
    }
}

impl servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if self.is_content(&webview) {
            self.chrome_eval(format!(
                "window.dispatchEvent(new CustomEvent('swerve:state',{{detail:{{url:{}}}}}))",
                js_string(url.as_str())
            ));
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if self.is_content(&webview) {
            self.chrome_eval(format!(
                "window.dispatchEvent(new CustomEvent('swerve:state',{{detail:{{title:{}}}}}))",
                js_string(title.as_deref().unwrap_or(""))
            ));
        }
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<Url>, current: usize) {
        if self.is_content(&webview) {
            let can_back = current > 0;
            let can_forward = current + 1 < entries.len();
            self.chrome_eval(format!(
                "window.dispatchEvent(new CustomEvent('swerve:state',{{detail:{{canGoBack:{can_back},canGoForward:{can_forward}}}}}))"
            ));
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
    Initial(Waker),
    Running(Rc<AppState>),
}

impl App {
    fn new(event_loop: &EventLoop<WakeUp>) -> Self {
        App::Initial(Waker(event_loop.create_proxy()))
    }
}

impl ApplicationHandler<WakeUp> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let App::Initial(waker) = self else { return };

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
        let chrome_h = (CHROME_HEIGHT_CSS as f64 * scale).round() as u32;

        let window_context = Rc::new(
            WindowRenderingContext::new(display_handle, window_handle, inner)
                .expect("failed to create WindowRenderingContext"),
        );
        let _ = window_context.make_current();

        let content_context =
            Rc::new(window_context.offscreen_context(content_size(inner, chrome_h)));

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker.clone()))
            .build();
        servo.setup_logging();

        let state = Rc::new(AppState {
            window,
            servo,
            window_context,
            content_context,
            chrome: RefCell::new(None),
            content: RefCell::new(None),
            chrome_height: Cell::new(chrome_h),
            cursor: Cell::new((0.0, 0.0)),
            focused: Cell::new(Focused::Content),
        });

        let chrome = WebViewBuilder::new(&state.servo, state.window_context.clone())
            .url(chrome_url())
            .hidpi_scale_factor(Scale::new(scale as f32))
            .delegate(state.clone())
            .build();

        let content = WebViewBuilder::new(&state.servo, state.content_context.clone())
            .url(content_url())
            .hidpi_scale_factor(Scale::new(scale as f32))
            .delegate(state.clone())
            .build();
        content.focus();

        *state.chrome.borrow_mut() = Some(chrome);
        *state.content.borrow_mut() = Some(content);
        *self = App::Running(state);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: WakeUp) {
        if let App::Running(state) = self {
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
                    let chrome_h = state.chrome_height.get() as f64;
                    state
                        .focused
                        .set(if y < chrome_h { Focused::Chrome } else { Focused::Content });
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

            WindowEvent::KeyboardInput { event: key_event, .. } => {
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

#[derive(Debug)]
struct WakeUp;

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let _ = self.0.send_event(WakeUp);
    }
}
