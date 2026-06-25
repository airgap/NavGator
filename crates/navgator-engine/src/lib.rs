//! navgator-engine — the single crate that depends on the Servo fork.
//!
//! Everything the rest of navgator uses from the engine is re-exported here, so the
//! `servo` / `embedder_traits` dependency (and the churn that comes with tracking a
//! forked engine) is quarantined to one place (ROADMAP §R2, `docs/FORK.md`).
//!
//! Today this is a thin re-export facade — the binary still works with Servo's own
//! types. The next step is to replace these re-exports with a servo-free navgator API
//! surface (defined alongside `navgator-protocol`), so a Servo type change touches only
//! this crate, not the app.

pub use servo::{
    CreateNewWebViewRequest, DeviceIntRect, DeviceIntSize, DevicePoint, InputEvent, Key, KeyState,
    KeyboardEvent, LoadStatus,
    MouseButton, MouseButtonAction,
    MouseButtonEvent, MouseMoveEvent, NamedKey, NavigationRequest, OffscreenRenderingContext,
    Preferences, RenderingContext, Servo, ServoBuilder,
    // Sandboxed multiprocess content: Opts carries the multiprocess/sandbox flags;
    // run_content_process is the entry point a re-exec'd content process hands off to.
    Opts, run_content_process,
    // OS sandbox (Landlock + seccomp) production policy builders, re-exported from
    // servo_constellation::sandbox_backend via servo. The --sandbox-selftest harness applies the
    // SAME policy create_sandbox() uses (plan §13.5, no drift): apply_sandbox(&content_process_policy()).
    SandboxOutcome, apply_sandbox, content_process_policy,
    // Userscript injection (UserContentManager-based) — gator-side feature, see main.rs.
    UserContentManager, UserScript,
    WebView, WebViewBuilder, WebViewDelegate,
    WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};

pub use servo::{
    AuthenticationRequest, ColorPicker, EmbedderControl, EmbedderControlId, FilePicker,
    FilterPattern, Image, MediaSessionEvent, MediaSessionPlaybackState, PermissionRequest,
    PixelFormat, RgbColor, SelectElement,
    SelectElementOption, SelectElementOptionOrOptgroup, SimpleDialog,
};

// Web-resource interception — used to serve gator:// internal pages without a net-internal
// ProtocolHandler. InterceptedWebResourceLoad / WebResourceRequest are returned/borrowed by
// callers and don't need to be named.
pub use servo::{WebResourceLoad, WebResourceResponse};

pub use embedder_traits::{EventLoopWaker, JSValue};

// `http` types (HeaderMap/StatusCode/HeaderValue), version-matched to the engine, for building
// the WebResourceResponse served to gator:// loads.
pub use http;
