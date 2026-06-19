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
    CreateNewWebViewRequest, DevicePoint, InputEvent, Key, KeyState, KeyboardEvent, LoadStatus,
    MouseButton, MouseButtonAction,
    MouseButtonEvent, MouseMoveEvent, NamedKey, NavigationRequest, OffscreenRenderingContext,
    RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder, WebViewDelegate, WheelDelta,
    WheelEvent, WheelMode, WindowRenderingContext,
};

pub use servo::{
    AuthenticationRequest, EmbedderControl, EmbedderControlId, SelectElement, SelectElementOption,
    SelectElementOptionOrOptgroup, SimpleDialog,
};

pub use embedder_traits::EventLoopWaker;
