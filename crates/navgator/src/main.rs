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
use egui_glow::{CallbackFn, EguiGlow};
use euclid::Scale;
use euclid::default::{Point2D, Rect, Size2D};
// Everything from the engine comes through navgator-engine, the only crate that touches
// the Servo fork (ROADMAP §R2; docs/FORK.md). IPC wire types come from navgator-protocol.
use navgator_engine::{
    AuthenticationRequest, ColorPicker, CreateNewWebViewRequest, DevicePoint, EmbedderControl,
    EmbedderControlId, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent, LoadStatus,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey as ServoNamedKey, NavigationRequest, OffscreenRenderingContext, RenderingContext,
    RgbColor, SelectElement, SelectElementOptionOrOptgroup, Servo, ServoBuilder, SimpleDialog,
    WebView, WebViewBuilder, WebViewDelegate, WheelDelta, WheelEvent, WheelMode,
    WindowRenderingContext,
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
    /// UI accent color (any CSS-style `#rrggbb`).
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

/// Truncate a tab title to `max` chars with an ellipsis.
fn truncate_ellipsis(input: &str, max: usize) -> String {
    if input.chars().count() > max {
        let t: String = input.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    } else {
        input.to_string()
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
    fullscreen: Cell<bool>,
    scale: Cell<f64>,
    cursor: Cell<(f64, f64)>,
    ctrl: Cell<bool>,
    shift: Cell<bool>,
    weak_self: RefCell<Weak<AppState>>,
    ipc_clients: Arc<Mutex<Vec<UnixStream>>>,
    settings: RefCell<Settings>,
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
            if !self.fullscreen.get() {
                self.draw_chrome(ctx);
            } else {
                self.toolbar_height.set(0.0);
            }
            self.draw_settings(ctx);
            self.draw_dialogs(ctx);

            // The page occupies everything below the chrome panels. (At the Context
            // level egui's available_rect doesn't reflect panel reservations, so derive
            // the content rect from the toolbar height measured during draw_chrome.)
            let top = self.toolbar_height.get();
            let screen = ctx.screen_rect();
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

    /// Toolbar (nav + address + window controls) and the tab strip.
    fn draw_chrome(&self, ctx: &egui::Context) {
        let frame = egui::Frame::default()
            .fill(ctx.style().visuals.window_fill)
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
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let raw = loc.trim().to_string();
                        drop(loc);
                        self.navigate_from_omnibox(&raw);
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
                            let title = self.tabs.borrow()[i].title.clone();
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
        self.toolbar_height.set(outer.response.rect.max.y);
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
                ui.label("Search engine (use %s for the query)");
                changed |= ui.text_edit_singleline(&mut s.search).changed();
                ui.add_space(6.0);
                ui.label("Accent color (#rrggbb)");
                changed |= ui.text_edit_singleline(&mut s.accent).changed();
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

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].url = url.to_string();
            self.ipc_emit(&format!("url {i} {url}"));
            if i == self.active.get() && !self.location_dirty.get() {
                *self.location.borrow_mut() = url.to_string();
            }
            self.window.request_redraw();
        }
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if let Some(i) = self.tab_index(&webview) {
            let title = title.unwrap_or_else(|| "New tab".to_string());
            self.ipc_emit(&format!("title {i} {title}"));
            self.tabs.borrow_mut()[i].title = title;
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

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if let Some(i) = self.tab_index(&webview) {
            self.tabs.borrow_mut()[i].loading = !matches!(status, LoadStatus::Complete);
            if matches!(status, LoadStatus::Complete) && i == self.active.get() {
                self.location_dirty.set(false);
            }
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
            // File picker, IME: not yet implemented.
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
            fullscreen: Cell::new(false),
            scale: Cell::new(scale),
            cursor: Cell::new((0.0, 0.0)),
            ctrl: Cell::new(false),
            shift: Cell::new(false),
            weak_self: RefCell::new(Weak::new()),
            ipc_clients,
            settings: RefCell::new(load_settings()),
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
                            state.new_tab(content_url());
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
