//! swerve — a web browser whose UI ("chrome") is HTML rendered by Servo.
//!
//! ## Milestone 2 (this file): chrome + content compositing
//! Two webviews in one window:
//!   * **chrome** — our HTML UI, rendered into the window's `WindowRenderingContext`
//!     (fills the window).
//!   * **content** — the web page, rendered into an `OffscreenRenderingContext`
//!     (an FBO), then composited into the content region *below* the chrome via
//!     `OffscreenRenderingContext::render_to_parent_callback()` (a scissor-clear +
//!     `blit_framebuffer`, GL bottom-left coords).
//!
//! This is the pattern servoshell's `Gui`/minibrowser uses for its egui toolbar —
//! we just swap egui for a second Servo webview. API verified against servo rev
//! `ed1af70` (`winit_minimal` + `ports/servoshell/desktop/gui.rs` +
//! `components/shared/paint/rendering_context.rs`).
//!
//! ### The chrome/content split
//! Until M3's chrome↔embedder bridge lets the chrome report its own content rect,
//! Rust and the chrome agree on a fixed split: [`CHROME_HEIGHT_CSS`] here must equal
//! the chrome header height in `src/chrome/chrome.css` (`.chrome { height: ... }`).
//!
//! ### Input
//! Mouse (move/button/wheel) is routed by region: chrome owns `y < CHROME_HEIGHT`,
//! content owns the rest (point shifted up by the chrome height). Clicking a region
//! focuses it. Keyboard is deferred until the M3 navigation bridge (it needs the
//! winit→keyboard_types mapping servoshell keeps in `keyutils.rs`).
//! Pass a URL arg to choose the content page: `cargo run -- https://servo.org`.

use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
use servo::{
    DevicePoint, InputEvent, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent,
    MouseMoveEvent, OffscreenRenderingContext, RenderingContext, Servo, ServoBuilder, WebView,
    WebViewBuilder, WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

/// Logical-pixel height of the HTML chrome (tabstrip + toolbar). MUST match
/// `.chrome { height: ... }` in `src/chrome/chrome.css`. See module docs.
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

/// The chrome UI document.
fn chrome_url() -> Url {
    file_url("src/chrome/index.html")
}

/// The content page: a CLI arg if given, else swerve's local new-tab page.
fn content_url() -> Url {
    if let Some(arg) = env::args().nth(1) {
        if let Ok(url) = Url::parse(&arg) {
            return url;
        }
        eprintln!("swerve: '{arg}' is not a valid URL, loading the home page instead");
    }
    file_url("src/content/home.html")
}

/// Physical-pixel size of the content region (window minus the chrome strip).
fn content_size(window: PhysicalSize<u32>, chrome_h: u32) -> PhysicalSize<u32> {
    PhysicalSize::new(
        window.width.max(1),
        window.height.saturating_sub(chrome_h).max(1),
    )
}

struct AppState {
    window: Window,
    servo: Servo,
    /// The OS-window surface; the chrome renders here and we composite into it.
    window_context: Rc<WindowRenderingContext>,
    /// The content webview's offscreen FBO; blitted into the window each frame.
    content_context: Rc<OffscreenRenderingContext>,
    chrome: RefCell<Option<WebView>>,
    content: RefCell<Option<WebView>>,
    /// Physical-pixel chrome height (logical * hidpi scale).
    chrome_height: Cell<u32>,
    /// Last known cursor position in physical window coordinates.
    cursor: Cell<(f64, f64)>,
}

impl AppState {
    /// Paint chrome → window FBO, content → offscreen FBO, then blit content into
    /// the window's content region and present.
    fn render(&self) {
        let chrome = self.chrome.borrow();
        let content = self.content.borrow();
        let (Some(chrome), Some(content)) = (chrome.as_ref(), content.as_ref()) else {
            return;
        };

        // Content into its own offscreen framebuffer.
        let _ = self.content_context.make_current();
        content.paint();

        // Chrome into the window framebuffer (fills the window).
        let _ = self.window_context.make_current();
        self.window_context.prepare_for_rendering();
        chrome.paint();

        // Composite the content FBO into the window's content region. The callback
        // scissor-clears `target_rect` then blits the offscreen FBO into it. Coords
        // are GL bottom-left, so the content region (the lower part of the window,
        // below the chrome strip) sits at origin (0, 0).
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
    /// webview's own coordinate space. Chrome owns `y < chrome_height`; content owns
    /// the rest (shifted up by the chrome height).
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
}

impl servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
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

        // Offscreen FBO for the content webview, sized to the content region.
        let content_context = Rc::new(window_context.offscreen_context(content_size(inner, chrome_h)));

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
                if let Some((webview, point)) = state.route(x, y) {
                    let action = match button_state {
                        ElementState::Pressed => MouseButtonAction::Down,
                        ElementState::Released => MouseButtonAction::Up,
                    };
                    let button = match button {
                        MouseButton::Left => ServoMouseButton::Left,
                        MouseButton::Right => ServoMouseButton::Right,
                        MouseButton::Middle => ServoMouseButton::Middle,
                        MouseButton::Back => ServoMouseButton::Back,
                        MouseButton::Forward => ServoMouseButton::Forward,
                        MouseButton::Other(v) => ServoMouseButton::Other(v),
                    };
                    // Clicking a region gives it keyboard focus.
                    if matches!(button_state, ElementState::Pressed) {
                        webview.focus();
                    }
                    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                        action, button, point.into(),
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

            // Keyboard is deferred: winit KeyEvent → keyboard_types::KeyboardEvent needs the
            // mapping servoshell keeps in keyutils.rs, and isn't useful until the M3
            // navigation bridge lets the omnibox act on typed input.
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
