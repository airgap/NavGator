//! Pure command-palette module for the ⌘K omnibar dropdown.
//!
//! This module has no dependency on `AppState`: it just produces the command
//! catalog and renders the floating dropdown, returning the action a clicked
//! row carries. The integrator owns filtering and dispatch.
#![allow(dead_code)]

use crate::theme::{Accent, Density, Palette, Preset, Wallpaper};
use egui::{Rect, Ui};

/// One command the palette can run. The integrator's dispatcher matches on this.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum PaletteAction {
    NewTab,
    ToggleVerticalTabs,
    ShrinkTabs,
    ToggleStudio,
    Density(Density),
    SetAccent(Accent),
    SetWallpaper(Wallpaper),
    ApplyPreset(Preset),
    ToggleNotes,
    ToggleFeed,
    OpenWhy,
    OpenExport,
}

/// The full command catalog: (label, right-aligned mono hint, action). `studio_open`
/// flips the studio label between "Open"/"Hide".
pub(crate) fn palette_catalog(studio_open: bool) -> Vec<(String, &'static str, PaletteAction)> {
    vec![
        ("New tab".to_string(), "⌘T", PaletteAction::NewTab),
        (
            "Show blocked requests (gator://why)".to_string(),
            "Privacy",
            PaletteAction::OpenWhy,
        ),
        (
            "Export my data (gator://export)".to_string(),
            "Data",
            PaletteAction::OpenExport,
        ),
        (
            "Toggle vertical tabs".to_string(),
            "Layout",
            PaletteAction::ToggleVerticalTabs,
        ),
        (
            "Shrink tabs to title width".to_string(),
            "Tabs",
            PaletteAction::ShrinkTabs,
        ),
        (
            if studio_open {
                "Hide customization studio"
            } else {
                "Open customization studio"
            }
            .to_string(),
            "Studio",
            PaletteAction::ToggleStudio,
        ),
        (
            "Density: Compact".to_string(),
            "",
            PaletteAction::Density(Density::Compact),
        ),
        (
            "Density: Cozy".to_string(),
            "",
            PaletteAction::Density(Density::Cozy),
        ),
        (
            "Accent: Violet".to_string(),
            "",
            PaletteAction::SetAccent(Accent::Violet),
        ),
        (
            "Accent: Cyan".to_string(),
            "",
            PaletteAction::SetAccent(Accent::Cyan),
        ),
        (
            // NOTE: label "Green" maps to Accent::Lime
            "Accent: Green".to_string(),
            "",
            PaletteAction::SetAccent(Accent::Lime),
        ),
        (
            "Accent: Magenta".to_string(),
            "",
            PaletteAction::SetAccent(Accent::Magenta),
        ),
        (
            "Wallpaper: Aurora".to_string(),
            "",
            PaletteAction::SetWallpaper(Wallpaper::Aurora),
        ),
        (
            "Wallpaper: Grid".to_string(),
            "",
            PaletteAction::SetWallpaper(Wallpaper::Grid),
        ),
        (
            "Wallpaper: Mesh".to_string(),
            "",
            PaletteAction::SetWallpaper(Wallpaper::Mesh),
        ),
        (
            "Apply preset: Aurora".to_string(),
            "",
            PaletteAction::ApplyPreset(Preset::Aurora),
        ),
        (
            "Apply preset: Terminal".to_string(),
            "",
            PaletteAction::ApplyPreset(Preset::Terminal),
        ),
        (
            "Apply preset: Halo".to_string(),
            "",
            PaletteAction::ApplyPreset(Preset::Halo),
        ),
        (
            "Apply preset: Noir".to_string(),
            "",
            PaletteAction::ApplyPreset(Preset::Noir),
        ),
        (
            "Toggle Notes widget".to_string(),
            "",
            PaletteAction::ToggleNotes,
        ),
        (
            "Toggle Reading list widget".to_string(),
            "",
            PaletteAction::ToggleFeed,
        ),
    ]
}

/// Render the palette dropdown as a floating Area anchored under `anchor` (the omnibar pill
/// rect). `items` is the already-filtered list. Returns the action of a clicked row, if any.
pub(crate) fn draw_palette_dropdown(
    ui: &mut Ui,
    anchor: Rect,
    items: &[(String, &'static str, PaletteAction)],
    pal: &Palette,
) -> Option<PaletteAction> {
    let mut clicked = None;

    egui::Area::new(egui::Id::new("command_palette"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 9.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::NONE
                .fill(pal.bg2)
                .stroke(egui::Stroke::new(1.0, pal.border))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(6))
                .shadow(egui::Shadow {
                    offset: [0, 12],
                    blur: 40,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(110),
                })
                .show(ui, |ui| {
                    let w = anchor.width().max(360.0);
                    ui.set_min_width(w);
                    ui.set_max_width(w);

                    if items.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("No commands match")
                                    .color(pal.muted)
                                    .size(13.0),
                            );
                        });
                        return;
                    }

                    for (i, (label, hint, action)) in items.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 32.0),
                            egui::Sense::click(),
                        );
                        let painter = ui.painter();

                        // Row background: active (first) row vs hovered.
                        if i == 0 {
                            painter.rect_filled(
                                rect,
                                egui::CornerRadius::same(9),
                                pal.accent_soft,
                            );
                            painter.rect_stroke(
                                rect,
                                egui::CornerRadius::same(9),
                                egui::Stroke::new(1.0, pal.accent_dim),
                                egui::StrokeKind::Inside,
                            );
                        } else if resp.hovered() {
                            painter.rect_filled(rect, egui::CornerRadius::same(9), pal.elev);
                        }

                        // 6px accent dot, ~13px from the left inside the row.
                        let dot_x = rect.left() + 13.0;
                        painter.circle_filled(
                            egui::pos2(dot_x, rect.center().y),
                            3.0,
                            pal.accent,
                        );

                        // Label, left-aligned after the dot.
                        let label_pos = egui::pos2(dot_x + 12.0, rect.center().y);
                        painter.text(
                            label_pos,
                            egui::Align2::LEFT_CENTER,
                            label,
                            egui::FontId::proportional(13.0),
                            pal.text,
                        );

                        // Hint, right-aligned monospace.
                        if !hint.is_empty() {
                            let right_pos = egui::pos2(rect.right() - 10.0, rect.center().y);
                            painter.text(
                                right_pos,
                                egui::Align2::RIGHT_CENTER,
                                hint,
                                egui::FontId::monospace(10.5),
                                pal.muted,
                            );
                        }

                        if resp.clicked() {
                            clicked = Some(*action);
                        }
                    }
                });
        });

    clicked
}
