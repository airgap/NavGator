//! Native new-tab dashboard, painted directly into the page content rect.
//!
//! This is the Rust replacement for the old `gator://welcome` HTML page: the
//! clock/greeting, search bar, top-site grid, and notes / reading-list cards
//! are all drawn with egui so nothing about the new-tab surface lives inside
//! the web engine. The module is intentionally *pure* — it depends only on the
//! [`theme`] tokens and the few owned buffers the integrator hands in, never on
//! the app's `AppState`. The frame loop calls [`draw_dashboard`] when the
//! active tab is a new tab and routes the returned navigation target.
#![allow(dead_code)]

use crate::theme::{self, Modules, Palette, Theme, Wallpaper};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect, RichText,
    Sense, Shadow, Stroke, StrokeKind,
};
use egui::{Context, Order};

/// Pre-formatted local clock strings (the integrator computes these with chrono).
pub(crate) struct DashClock {
    /// e.g. "TUESDAY, JUNE 23" (uppercase).
    pub date: String,
    /// e.g. "03:21 PM".
    pub time: String,
    /// "Good morning" | "Good afternoon" | "Good evening".
    pub greeting: &'static str,
}

/// One top-site tile: (display name, avatar letter, avatar hue, navigation domain).
struct Site {
    name: &'static str,
    letter: &'static str,
    hue: f32,
    domain: &'static str,
}

const SITES: &[Site] = &[
    Site { name: "Figma", letter: "F", hue: 200.0, domain: "figma.com" },
    Site { name: "GitHub", letter: "G", hue: 285.0, domain: "github.com" },
    Site { name: "Linear", letter: "L", hue: 270.0, domain: "linear.app" },
    Site { name: "Notion", letter: "N", hue: 40.0, domain: "notion.so" },
    Site { name: "Vercel", letter: "V", hue: 280.0, domain: "vercel.com" },
    Site { name: "Arc", letter: "A", hue: 330.0, domain: "arc.net" },
    Site { name: "Raycast", letter: "R", hue: 25.0, domain: "raycast.com" },
    Site { name: "Spotify", letter: "S", hue: 145.0, domain: "open.spotify.com" },
    Site { name: "Reader", letter: "R", hue: 55.0, domain: "getpocket.com" },
    Site { name: "Maps", letter: "M", hue: 230.0, domain: "maps.google.com" },
];

/// Notes-card sample rows: (text, done).
const NOTES: &[(&str, bool)] = &[
    ("Ship vertical-tabs experiment", true),
    ("Review Studio onboarding copy", false),
    ("Sync OKLCH token export", false),
];

/// Reading-list sample rows: (title, meta).
const READING: &[(&str, &str)] = &[
    ("The case for native browser chrome", "airgap.dev · 6 min"),
    ("OKLCH theming in practice", "evilmartians.com · 9 min"),
    ("Embedding Servo, lessons", "servo.org · 4 min"),
];

/// Paint the dashboard into `avail` (the page content rect). `search` is the
/// dashboard search-bar buffer (owned by the integrator). Returns a navigation
/// target (a URL/query string) if the user pressed Enter in the search bar
/// (non-empty) or clicked a top-site tile.
pub(crate) fn draw_dashboard(
    ctx: &Context,
    avail: Rect,
    theme: &Theme,
    modules: &Modules,
    pal: &Palette,
    clock: &DashClock,
    search: &mut String,
) -> Option<String> {
    let mut nav: Option<String> = None;

    egui::Area::new(Id::new("dashboard"))
        .order(Order::Middle)
        .fixed_pos(avail.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(avail);

            // --- Background ----------------------------------------------------
            paint_wallpaper(ui, avail, theme, pal);

            ui.set_width(avail.width());
            ui.set_height(avail.height());

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.set_max_width(900.0_f32.min(avail.width() - 32.0));
                    ui.add_space(60.0);

                    if modules.clock {
                        section_clock(ui, pal, clock);
                        ui.add_space(28.0);
                    }

                    if modules.search {
                        if let Some(target) = section_search(ui, theme, pal, search) {
                            nav = Some(target);
                        }
                        ui.add_space(28.0);
                    }

                    if modules.sites {
                        if let Some(target) = section_sites(ui, theme, pal) {
                            nav = Some(target);
                        }
                        ui.add_space(28.0);
                    }

                    if modules.notes || modules.feed {
                        section_notes_feed(ui, theme, pal, modules);
                    }

                    ui.add_space(80.0);
                });
            });
        });

    nav
}

// ---------------------------------------------------------------------------
// 1. Clock & greeting
// ---------------------------------------------------------------------------

fn section_clock(ui: &mut egui::Ui, pal: &Palette, clock: &DashClock) {
    // Left-align the block within the centered column.
    ui.with_layout(Layout::top_down(Align::Min), |ui| {
        ui.label(
            RichText::new(&clock.date)
                .font(FontId::monospace(12.0))
                .color(pal.muted),
        );
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&clock.time)
                    .size(66.0)
                    .strong()
                    .color(pal.text),
            );
            ui.add_space(14.0);
            ui.label(RichText::new(clock.greeting).size(22.0).color(pal.muted));
        });
    });
}

// ---------------------------------------------------------------------------
// 2. Search bar
// ---------------------------------------------------------------------------

fn section_search(
    ui: &mut egui::Ui,
    theme: &Theme,
    pal: &Palette,
    search: &mut String,
) -> Option<String> {
    let mut nav = None;

    let inner_pad = 14.0_f32;
    let card = Frame::NONE
        .fill(pal.bg2)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(theme.radius))
        .inner_margin(Margin::symmetric(16, inner_pad as i8))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(40),
        });

    card.show(ui, |ui| {
        ui.set_min_height(58.0 - inner_pad * 2.0);
        ui.horizontal(|ui| {
            ui.set_min_height(58.0 - inner_pad * 2.0);

            // Accent dot.
            let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), Sense::hover());
            ui.painter()
                .circle_filled(dot_rect.center(), 4.5, pal.accent);

            ui.add_space(10.0);

            // The keycap reserves a little room on the right; let the field take
            // the rest.
            let keycap_w = 36.0_f32;
            let field_w = (ui.available_width() - keycap_w - 8.0).max(60.0);

            let field = ui.add_sized(
                egui::vec2(field_w, 24.0),
                egui::TextEdit::singleline(search)
                    .id(Id::new("dash_search"))
                    .frame(Frame::NONE)
                    .hint_text("Search the web, your tabs, or type a command…")
                    .font(FontId::proportional(16.0)),
            );

            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let text = search.trim().to_string();
                if !text.is_empty() {
                    nav = Some(text);
                    search.clear();
                }
            }

            // "⌘K" keycap.
            Frame::NONE
                .stroke(Stroke::new(1.0, pal.border))
                .corner_radius(CornerRadius::same(theme.radius_sm()))
                .inner_margin(Margin::symmetric(6, 2))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⌘K")
                            .font(FontId::monospace(10.5))
                            .color(pal.muted),
                    );
                });
        });
    });

    nav
}

// ---------------------------------------------------------------------------
// 3. Top sites
// ---------------------------------------------------------------------------

fn section_sites(ui: &mut egui::Ui, theme: &Theme, pal: &Palette) -> Option<String> {
    let mut nav = None;

    ui.with_layout(Layout::top_down(Align::Min), |ui| {
        ui.label(
            RichText::new("TOP SITES")
                .size(12.0)
                .color(pal.muted),
        );
        ui.add_space(8.0);

        egui::Grid::new("dash_sites")
            .num_columns(5)
            .spacing([12.0, 12.0])
            .show(ui, |ui| {
                for (i, site) in SITES.iter().enumerate() {
                    if let Some(target) = site_tile(ui, theme, pal, site) {
                        nav = Some(target);
                    }
                    if (i + 1) % 5 == 0 {
                        ui.end_row();
                    }
                }
            });
    });

    nav
}

fn site_tile(ui: &mut egui::Ui, theme: &Theme, pal: &Palette, site: &Site) -> Option<String> {
    let card = Frame::NONE
        .fill(pal.bg2)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(theme.radius_sm()))
        .inner_margin(Margin::symmetric(8, 16));

    let inner = card.show(ui, |ui| {
        ui.vertical_centered(|ui| {
            // 42px rounded-square avatar in white letter.
            let (av_rect, _) =
                ui.allocate_exact_size(egui::vec2(42.0, 42.0), Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(
                av_rect,
                CornerRadius::same(13),
                theme::oklch(0.6, 0.16, site.hue),
            );
            painter.text(
                av_rect.center(),
                Align2::CENTER_CENTER,
                site.letter,
                FontId::proportional(18.0),
                Color32::WHITE,
            );

            ui.add_space(8.0);
            ui.label(RichText::new(site.name).size(12.0).color(pal.muted));
        });
    });

    // Make the whole tile clickable.
    let resp = inner.response.interact(Sense::click());
    if resp.hovered() {
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(theme.radius_sm()),
            Stroke::new(1.0, pal.accent_dim),
            StrokeKind::Inside,
        );
    }
    if resp.clicked() {
        return Some(site.domain.to_string());
    }

    None
}

// ---------------------------------------------------------------------------
// 4. Notes + Reading list
// ---------------------------------------------------------------------------

fn section_notes_feed(ui: &mut egui::Ui, theme: &Theme, pal: &Palette, modules: &Modules) {
    if modules.notes && modules.feed {
        ui.columns(2, |cols| {
            notes_card(&mut cols[0], theme, pal);
            reading_card(&mut cols[1], theme, pal);
        });
    } else if modules.notes {
        notes_card(ui, theme, pal);
    } else if modules.feed {
        reading_card(ui, theme, pal);
    }
}

fn panel_frame(theme: &Theme, pal: &Palette) -> Frame {
    Frame::NONE
        .fill(pal.bg2)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(theme.radius))
        .inner_margin(Margin::same(20))
}

fn notes_card(ui: &mut egui::Ui, theme: &Theme, pal: &Palette) {
    panel_frame(theme, pal).show(ui, |ui| {
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.label(RichText::new("Notes").size(13.0).strong().color(pal.text));
            ui.add_space(10.0);

            for (text, done) in NOTES {
                ui.horizontal(|ui| {
                    // 16px rounded checkbox.
                    let (box_rect, _) =
                        ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
                    let painter = ui.painter();
                    let cr = CornerRadius::same(4);
                    if *done {
                        painter.rect_filled(box_rect, cr, pal.accent);
                    }
                    painter.rect_stroke(
                        box_rect,
                        cr,
                        Stroke::new(1.5, pal.accent_dim),
                        StrokeKind::Inside,
                    );
                    if *done {
                        painter.text(
                            box_rect.center(),
                            Align2::CENTER_CENTER,
                            "✓",
                            FontId::proportional(11.0),
                            Color32::WHITE,
                        );
                    }

                    ui.add_space(8.0);
                    ui.label(RichText::new(*text).size(13.0).color(pal.muted));
                });
                ui.add_space(6.0);
            }
        });
    });
}

fn reading_card(ui: &mut egui::Ui, theme: &Theme, pal: &Palette) {
    panel_frame(theme, pal).show(ui, |ui| {
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.label(
                RichText::new("Reading list")
                    .size(13.0)
                    .strong()
                    .color(pal.text),
            );
            ui.add_space(10.0);

            for (i, (title, meta)) in READING.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.label(RichText::new(*title).size(13.5).color(pal.text));
                ui.add_space(2.0);
                ui.label(
                    RichText::new(*meta)
                        .font(FontId::monospace(11.0))
                        .color(pal.muted),
                );
                ui.add_space(4.0);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Wallpaper background
// ---------------------------------------------------------------------------

fn paint_wallpaper(ui: &mut egui::Ui, avail: Rect, theme: &Theme, pal: &Palette) {
    let painter = ui.painter();
    painter.rect_filled(avail, CornerRadius::ZERO, pal.bg);

    match theme.wallpaper {
        Wallpaper::Grid => {
            let step = 30.0_f32;
            let line = Color32::from_white_alpha(9);
            // Cap the loop so an absurd rect can't blow up paint time.
            let max_lines = 400_i32;

            let mut x = avail.left();
            let mut n = 0;
            while x <= avail.right() && n < max_lines {
                painter.line_segment(
                    [Pos2::new(x, avail.top()), Pos2::new(x, avail.bottom())],
                    Stroke::new(1.0, line),
                );
                x += step;
                n += 1;
            }

            let mut y = avail.top();
            n = 0;
            while y <= avail.bottom() && n < max_lines {
                painter.line_segment(
                    [Pos2::new(avail.left(), y), Pos2::new(avail.right(), y)],
                    Stroke::new(1.0, line),
                );
                y += step;
                n += 1;
            }
        }
        Wallpaper::Aurora => {
            // One large soft accent glow near the bottom-right.
            let center = Pos2::new(avail.right() - 160.0, avail.bottom() - 120.0);
            let (r, g, b) = oklch_rgb(0.6, 0.14, 250.0);
            for radius in [320.0_f32, 220.0, 120.0] {
                painter.circle_filled(
                    center,
                    radius,
                    Color32::from_rgba_unmultiplied(r, g, b, 12),
                );
            }
        }
        Wallpaper::Mesh => {
            // Two soft blobs at opposite corners.
            let cyan = oklch_rgb(0.62, 0.16, 200.0);
            let mag = oklch_rgb(0.6, 0.18, 340.0);
            let c1 = Pos2::new(avail.left() + 140.0, avail.top() + 120.0);
            let c2 = Pos2::new(avail.right() - 140.0, avail.bottom() - 120.0);
            for radius in [300.0_f32, 200.0, 110.0] {
                painter.circle_filled(
                    c1,
                    radius,
                    Color32::from_rgba_unmultiplied(cyan.0, cyan.1, cyan.2, 11),
                );
                painter.circle_filled(
                    c2,
                    radius,
                    Color32::from_rgba_unmultiplied(mag.0, mag.1, mag.2, 11),
                );
            }
        }
        Wallpaper::Mono => {
            // Just the bg fill.
        }
    }
}

/// Extract the sRGB triple of an OKLCH color (via the opaque [`theme::oklch`]).
fn oklch_rgb(l: f32, c: f32, h: f32) -> (u8, u8, u8) {
    let col = theme::oklch(l, c, h);
    (col.r(), col.g(), col.b())
}
