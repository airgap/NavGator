//! The body of the "Studio" customization panel, drawn in pure egui.
//!
//! [`studio_body`] renders the scrollable panel contents (presets, accent,
//! surface, typography, tab placement, new-tab modules) and mutates the
//! supplied [`Theme`] / [`Modules`] in place. The integrator wires the side
//! panel, header, close button, and scroll area around it.
#![allow(dead_code)]

use crate::theme::{
    self, family, Base, Density, FontChoice, Modules, Palette, Preset, TabFit, TabPos, Theme,
};
use crate::widgets;

/// Vertical gap between top-level sections (~24px per spec).
const SECTION_GAP: f32 = 24.0;

/// Render the Studio panel body. Returns `true` if anything changed this frame.
pub(crate) fn studio_body(
    ui: &mut egui::Ui,
    theme: &mut Theme,
    modules: &mut Modules,
    pal: &Palette,
) -> bool {
    let mut changed = false;

    changed |= section_presets(ui, theme, pal);
    ui.add_space(SECTION_GAP);

    changed |= section_accent(ui, theme, pal);
    ui.add_space(SECTION_GAP);

    changed |= section_surface(ui, theme, pal);
    ui.add_space(SECTION_GAP);

    changed |= section_typography(ui, theme, pal);
    ui.add_space(SECTION_GAP);

    changed |= section_tab_placement(ui, theme, pal);
    ui.add_space(SECTION_GAP);

    changed |= section_modules(ui, modules, pal);

    changed
}

// ---------------------------------------------------------------------------
// Section helpers
// ---------------------------------------------------------------------------

/// Uppercase 11px muted section heading.
fn section_label(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    ui.label(egui::RichText::new(text).size(11.0).color(pal.muted));
    ui.add_space(8.0);
}

/// 1. Presets — 2-column grid of clickable preset cards.
fn section_presets(ui: &mut egui::Ui, theme: &mut Theme, pal: &Palette) -> bool {
    section_label(ui, "PRESETS", pal);
    let mut changed = false;

    // egui::Grid sizes column 0 to its content's *minimum* width, which — for a card whose
    // label can wrap — collapses to ~1 character (the title then renders one letter per line)
    // while column 1 takes all remaining space. Pin both columns to an equal half of the
    // available panel width so each card gets a real, symmetric width.
    let col_w = ((ui.available_width() - 9.0) / 2.0 - 1.0).max(60.0);
    egui::Grid::new("studio_presets")
        .num_columns(2)
        .spacing([9.0, 9.0])
        .min_col_width(col_w)
        .max_col_width(col_w)
        .show(ui, |ui| {
            for (i, preset) in Preset::ALL.iter().copied().enumerate() {
                let (c1, c2) = preset.swatch();

                let inner = widgets::card_frame(pal.bg, pal.border, 11).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        widgets::gradient_swatch(ui, 22.0, c1, c2);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(preset.label()).size(12.5));
                            ui.label(
                                egui::RichText::new(preset.sub_label())
                                    .monospace()
                                    .size(10.5)
                                    .color(pal.muted),
                            );
                        });
                    });
                });

                let resp = inner.response.interact(egui::Sense::click());
                if resp.clicked() {
                    preset.merge_into(theme);
                    changed = true;
                }
                if resp.hovered() {
                    ui.painter().rect_stroke(
                        resp.rect,
                        egui::CornerRadius::same(11),
                        egui::Stroke::new(1.0, pal.accent_dim),
                        egui::StrokeKind::Inside,
                    );
                }

                if i % 2 == 1 {
                    ui.end_row();
                }
            }
            // Close any trailing row (ALL has an even count, but be safe).
            if Preset::ALL.len() % 2 == 1 {
                ui.end_row();
            }
        });

    changed
}

/// 2. Accent — circular swatches of every accent valid for the base.
fn section_accent(ui: &mut egui::Ui, theme: &mut Theme, pal: &Palette) -> bool {
    section_label(ui, "ACCENT", pal);
    let mut changed = false;

    ui.horizontal_wrapped(|ui| {
        for accent in Theme::accents_for_base(theme.base) {
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::click());
            if ui.is_rect_visible(rect) {
                let (l, c, h) = accent.lch();
                let color = theme::oklch(l, c, h);
                let center = rect.center();
                let painter = ui.painter();
                painter.circle_filled(center, 16.0, color);
                if accent == theme.accent {
                    painter.circle_stroke(center, 17.0, egui::Stroke::new(2.0, pal.bg2));
                    painter.circle_stroke(center, 19.0, egui::Stroke::new(2.0, pal.accent));
                }
            }
            if resp.clicked() && accent != theme.accent {
                theme.accent = accent;
                changed = true;
            }
        }
    });

    changed
}

/// 3. Surface — base chips + corner-radius / glass-blur sliders.
fn section_surface(ui: &mut egui::Ui, theme: &mut Theme, pal: &Palette) -> bool {
    section_label(ui, "SURFACE", pal);
    let mut changed = false;

    // `horizontal_wrapped` fails to wrap Frame-wrapped chips (the frame's size isn't known
    // when the wrap decision is made), so all six bases laid out on one row and ran off the
    // panel's right edge. Use a fixed 3-column grid of equal width instead. See section_presets.
    let surf_cols = 3;
    let surf_w = ((ui.available_width() - (surf_cols - 1) as f32 * 9.0) / surf_cols as f32
        - 1.0)
        .max(50.0);
    egui::Grid::new("studio_surface")
        .num_columns(surf_cols)
        .spacing([9.0, 9.0])
        .min_col_width(surf_w)
        .max_col_width(surf_w)
        .show(ui, |ui| {
            for (i, base) in Base::ALL.iter().copied().enumerate() {
                // Resolve this base's bg2 by building a temp theme.
                let bg2 = Theme { base, ..*theme }.palette().bg2;

                let inner = widgets::card_frame(pal.bg, pal.border, 9).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (sq, _) =
                            ui.allocate_exact_size(egui::vec2(13.0, 13.0), egui::Sense::hover());
                        ui.painter()
                            .rect_filled(sq, egui::CornerRadius::same(3), bg2);
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(base.label()).size(11.5));
                    });
                });

                let resp = inner.response.interact(egui::Sense::click());
                if base == theme.base {
                    ui.painter().rect_stroke(
                        resp.rect,
                        egui::CornerRadius::same(9),
                        egui::Stroke::new(1.5, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                if resp.clicked() && base != theme.base {
                    theme.set_base(base);
                    changed = true;
                }

                if i % surf_cols == surf_cols - 1 {
                    ui.end_row();
                }
            }
        });

    ui.add_space(10.0);

    // Corner radius slider.
    ui.horizontal(|ui| {
        ui.label("Corner radius");
        let resp = ui.add(egui::Slider::new(&mut theme.radius, 0..=30).show_value(false));
        changed |= resp.changed();
        ui.label(egui::RichText::new(format!("{}px", theme.radius)).monospace());
    });

    ui.add_space(6.0);

    // Glass blur slider.
    ui.horizontal(|ui| {
        ui.label("Glass blur");
        let resp = ui.add(egui::Slider::new(&mut theme.glass, 0..=60).show_value(false));
        changed |= resp.changed();
        ui.label(egui::RichText::new(format!("{}px", theme.glass)).monospace());
    });

    changed
}

/// 4. Typography — font tiles + density segmented control.
fn section_typography(ui: &mut egui::Ui, theme: &mut Theme, pal: &Palette) -> bool {
    section_label(ui, "TYPOGRAPHY", pal);
    let mut changed = false;

    // A plain `ui.horizontal` let each card's `vertical_centered` body grab the full panel
    // width, so the first font tile ballooned across (and past) the panel and pushed the rest
    // off-screen. Pin the tiles to equal columns so each one is bounded.
    let font_cols = FontChoice::ALL.len();
    let font_w = ((ui.available_width() - (font_cols.saturating_sub(1)) as f32 * 9.0)
        / font_cols as f32
        - 1.0)
        .max(50.0);
    egui::Grid::new("studio_fonts")
        .num_columns(font_cols)
        .spacing([9.0, 9.0])
        .min_col_width(font_w)
        .max_col_width(font_w)
        .show(ui, |ui| {
            for f in FontChoice::ALL.iter().copied() {
                let inner = widgets::card_frame(pal.bg, pal.border, 9).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Aa")
                                .family(family(f))
                                .size(20.0),
                        );
                        ui.add_space(2.0);
                        ui.label(egui::RichText::new(f.label()).size(11.0));
                    });
                });

                let resp = inner.response.interact(egui::Sense::click());
                if f == theme.font {
                    ui.painter().rect_stroke(
                        resp.rect,
                        egui::CornerRadius::same(9),
                        egui::Stroke::new(1.5, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                if resp.clicked() && f != theme.font {
                    theme.font = f;
                    changed = true;
                }
            }
        });

    ui.add_space(10.0);

    // Density segmented control.
    let mut d = theme.density;
    if widgets::segmented_control(
        ui,
        &mut d,
        &[(Density::Compact, "Compact"), (Density::Cozy, "Cozy")],
        pal.accent_soft,
        pal.accent_dim,
    ) {
        theme.density = d;
        changed = true;
    }

    changed
}

/// 5. Tab placement — position segmented control + (Top-only) fit + max width.
fn section_tab_placement(ui: &mut egui::Ui, theme: &mut Theme, pal: &Palette) -> bool {
    section_label(ui, "TAB PLACEMENT", pal);
    let mut changed = false;

    let mut pos = theme.tab_pos;
    if widgets::segmented_control(
        ui,
        &mut pos,
        &[(TabPos::Top, "Top bar"), (TabPos::Left, "Sidebar")],
        pal.accent_soft,
        pal.accent_dim,
    ) {
        theme.tab_pos = pos;
        changed = true;
    }

    if theme.tab_pos == TabPos::Top {
        ui.add_space(10.0);

        // Shrink tabs to title.
        ui.horizontal(|ui| {
            ui.label("Shrink tabs to title");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = theme.tab_fit == TabFit::Fit;
                if widgets::toggle_switch(ui, &mut on, pal.accent, pal.elev).changed() {
                    theme.tab_fit = if on { TabFit::Fit } else { TabFit::Fill };
                    changed = true;
                }
            });
        });

        ui.add_space(6.0);

        // Max tab width.
        ui.horizontal(|ui| {
            ui.label("Max tab width");
            let resp =
                ui.add(egui::Slider::new(&mut theme.tab_max_w, 120..=340).show_value(false));
            changed |= resp.changed();
            ui.label(egui::RichText::new(format!("{}px", theme.tab_max_w)).monospace());
        });
    }

    changed
}

/// 6. New-tab modules — five label + toggle rows.
fn section_modules(ui: &mut egui::Ui, modules: &mut Modules, pal: &Palette) -> bool {
    section_label(ui, "NEW-TAB MODULES", pal);
    let mut changed = false;

    let rows: [(&str, fn(&mut Modules) -> &mut bool); 5] = [
        ("Clock & greeting", |m| &mut m.clock),
        ("Search bar", |m| &mut m.search),
        ("Top sites", |m| &mut m.sites),
        ("Notes", |m| &mut m.notes),
        ("Reading list", |m| &mut m.feed),
    ];

    for (i, (label, field)) in rows.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(*label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let flag = field(modules);
                if widgets::toggle_switch(ui, flag, pal.accent, pal.elev).changed() {
                    changed = true;
                }
            });
        });
        if i + 1 < rows.len() {
            ui.add_space(2.0);
        }
    }

    changed
}
