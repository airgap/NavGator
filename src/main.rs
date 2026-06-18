//! swerve — a web browser whose UI ("chrome") is HTML rendered by Servo.
//!
//! ## Milestone 1 (this file)
//! Stand up libservo in a winit window and render a SINGLE webview. By default it
//! loads the local HTML chrome (`src/chrome/index.html`) so you can watch Servo
//! paint your own UI. Pass a URL as the first CLI arg to load a page instead:
//!
//! ```text
//! cargo run -- https://servo.org
//! ```
//!
//! The point of Milestone 1 is to get the Servo build, the toolchain, and the
//! event loop GREEN before taking on compositing. Getting Servo to build and run
//! at all is the step that historically sinks Servo-embedding projects (see
//! `docs/ARCHITECTURE.md` — "the Verso lesson").
//!
//! ## Milestone 2 (not here yet)
//! Two regions in one window — HTML chrome on top, web content below — composited
//! via an `OffscreenRenderingContext`, the same mechanism servoshell's Minibrowser
//! uses for its toolbar. See `docs/ARCHITECTURE.md`.
//!
//! API verified against servo rev `ed1af70` (its `winit_minimal` example).

use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use euclid::Scale;
use servo::{
    RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder, WindowRenderingContext,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

fn main() -> Result<(), Box<dyn Error>> {
    // Servo speaks TLS through rustls; a crypto provider must be installed before
    // any network activity.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let event_loop = EventLoop::with_user_event().build()?;
    let mut app = App::new(&event_loop);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Where the (single, Milestone-1) webview points on startup.
fn startup_url() -> Url {
    if let Some(arg) = env::args().nth(1) {
        if let Ok(url) = Url::parse(&arg) {
            return url;
        }
        eprintln!("swerve: '{arg}' is not a valid URL, loading chrome instead");
    }
    // Default: render our own HTML chrome, so M1 demonstrates "Servo paints our UI".
    let chrome = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/chrome/index.html");
    Url::from_file_path(&chrome).unwrap_or_else(|_| Url::parse("about:blank").unwrap())
}

/// Everything the running app needs. Shared (via `Rc`) so it can also serve as the
/// per-webview delegate.
struct AppState {
    window: Window,
    servo: Servo,
    rendering_context: Rc<WindowRenderingContext>,
    webviews: RefCell<Vec<WebView>>,
}

// Servo drives the embedder through delegate callbacks. The default `WebViewDelegate`
// already does the sensible thing (e.g. allows navigation), so we override only what
// we need. The big set of hooks we'll implement in M2 lives here too: title/URL
// changes (to update the chrome), new-window/popup requests, prompts, context menus.
impl servo::WebViewDelegate for AppState {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        // A new frame is ready to paint — ask winit to redraw.
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
            .create_window(Window::default_attributes().with_title("swerve"))
            .expect("failed to create window");
        let window_handle = window.window_handle().expect("no window handle");

        // One rendering context bound to the OS window. In M2, content webviews get
        // their own OffscreenRenderingContext instead and we composite into this one.
        let rendering_context = Rc::new(
            WindowRenderingContext::new(display_handle, window_handle, window.inner_size())
                .expect("failed to create WindowRenderingContext"),
        );
        let _ = rendering_context.make_current();

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker.clone()))
            .build();
        servo.setup_logging();

        let state = Rc::new(AppState {
            window,
            servo,
            rendering_context,
            webviews: RefCell::new(Vec::new()),
        });

        let webview = WebViewBuilder::new(&state.servo, state.rendering_context.clone())
            .url(startup_url())
            .hidpi_scale_factor(Scale::new(state.window.scale_factor() as f32))
            .delegate(state.clone())
            .build();
        webview.focus();
        state.webviews.borrow_mut().push(webview);

        *self = App::Running(state);
    }

    /// Servo woke us up (new frame, network I/O, timer, …) — let it make progress.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: WakeUp) {
        if let App::Running(state) = self {
            state.servo.spin_event_loop();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let App::Running(state) = self else { return };

        // Servo needs a chance to process the latest messages on every wake.
        state.servo.spin_event_loop();

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(webview) = state.webviews.borrow().last() {
                    webview.paint();
                }
                state.rendering_context.present();
            }
            WindowEvent::Resized(size) => {
                // NOTE: the verified winit_minimal example resizes only the webview.
                // A WindowRenderingContext likely also wants `.resize(size)` here;
                // confirm against the pinned rev once it builds.
                if let Some(webview) = state.webviews.borrow().last() {
                    webview.resize(size);
                }
            }
            // M2: route mouse/keyboard via `webview.notify_input_event(..)`, deciding
            // chrome-vs-content by which region the pointer is in. winit_minimal shows
            // the wheel-event shape.
            _ => {}
        }
    }
}

/// Bridges Servo's "wake the UI thread" requests onto the winit event loop.
#[derive(Clone)]
struct Waker(EventLoopProxy<WakeUp>);

/// The user event we post to wake winit. (Unit struct — it only signals "spin".)
#[derive(Debug)]
struct WakeUp;

impl embedder_traits::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn embedder_traits::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        // If the loop is already gone, dropping the event is fine.
        let _ = self.0.send_event(WakeUp);
    }
}
