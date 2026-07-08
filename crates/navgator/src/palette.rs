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
    ToggleForceDark,
    ToggleSiteDark,
    ToggleSiteAdblock,
    ReaderMode,
    NewWorkspace,
    NextWorkspace,
    MoveTabToNextWorkspace,
    ManageWorkspaces,
}

/// The full command catalog: (label, right-aligned mono hint, action).
pub(crate) fn palette_catalog() -> Vec<(String, String, PaletteAction)> {
    vec![
        (tr!("tab-new"), "⌘T".to_string(), PaletteAction::NewTab),
        (
            tr!("palette-cmd-blocked"),
            tr!("palette-cat-privacy"),
            PaletteAction::OpenWhy,
        ),
        (
            tr!("palette-cmd-export"),
            tr!("palette-cat-data"),
            PaletteAction::OpenExport,
        ),
        (
            tr!("palette-cmd-force-dark"),
            tr!("palette-cat-appearance"),
            PaletteAction::ToggleForceDark,
        ),
        (
            tr!("palette-cmd-site-dark"),
            tr!("palette-cat-loadout"),
            PaletteAction::ToggleSiteDark,
        ),
        (
            tr!("palette-cmd-site-adblock"),
            tr!("palette-cat-loadout"),
            PaletteAction::ToggleSiteAdblock,
        ),
        (
            tr!("palette-cmd-reader"),
            tr!("palette-cat-reading"),
            PaletteAction::ReaderMode,
        ),
        (
            tr!("palette-cmd-new-workspace"),
            tr!("palette-cat-spaces"),
            PaletteAction::NewWorkspace,
        ),
        (
            tr!("palette-cmd-next-workspace"),
            "⇧⌘E".to_string(),
            PaletteAction::NextWorkspace,
        ),
        (
            tr!("palette-cmd-move-workspace"),
            tr!("palette-cat-spaces"),
            PaletteAction::MoveTabToNextWorkspace,
        ),
        (
            tr!("palette-cmd-manage-workspaces"),
            tr!("palette-cat-spaces"),
            PaletteAction::ManageWorkspaces,
        ),
        (
            tr!("palette-cmd-vertical-tabs"),
            tr!("palette-cat-layout"),
            PaletteAction::ToggleVerticalTabs,
        ),
        (
            tr!("palette-cmd-shrink-tabs"),
            tr!("palette-cat-tabs"),
            PaletteAction::ShrinkTabs,
        ),
        (
            tr!("palette-cmd-customize"),
            tr!("palette-cat-settings"),
            PaletteAction::ToggleStudio,
        ),
        (
            tr!("palette-cmd-density-compact"),
            String::new(),
            PaletteAction::Density(Density::Compact),
        ),
        (
            tr!("palette-cmd-density-cozy"),
            String::new(),
            PaletteAction::Density(Density::Cozy),
        ),
        (
            tr!("palette-cmd-accent-violet"),
            String::new(),
            PaletteAction::SetAccent(Accent::Violet),
        ),
        (
            tr!("palette-cmd-accent-cyan"),
            String::new(),
            PaletteAction::SetAccent(Accent::Cyan),
        ),
        (
            // NOTE: label "Green" maps to Accent::Lime
            tr!("palette-cmd-accent-green"),
            String::new(),
            PaletteAction::SetAccent(Accent::Lime),
        ),
        (
            tr!("palette-cmd-accent-magenta"),
            String::new(),
            PaletteAction::SetAccent(Accent::Magenta),
        ),
        (
            tr!("palette-cmd-wp-aurora"),
            String::new(),
            PaletteAction::SetWallpaper(Wallpaper::Aurora),
        ),
        (
            tr!("palette-cmd-wp-grid"),
            String::new(),
            PaletteAction::SetWallpaper(Wallpaper::Grid),
        ),
        (
            tr!("palette-cmd-wp-mesh"),
            String::new(),
            PaletteAction::SetWallpaper(Wallpaper::Mesh),
        ),
        (
            tr!("palette-cmd-preset-aurora"),
            String::new(),
            PaletteAction::ApplyPreset(Preset::Aurora),
        ),
        (
            tr!("palette-cmd-preset-terminal"),
            String::new(),
            PaletteAction::ApplyPreset(Preset::Terminal),
        ),
        (
            tr!("palette-cmd-preset-halo"),
            String::new(),
            PaletteAction::ApplyPreset(Preset::Halo),
        ),
        (
            tr!("palette-cmd-preset-noir"),
            String::new(),
            PaletteAction::ApplyPreset(Preset::Noir),
        ),
        (
            "Toggle Notes widget".to_string(),
            String::new(),
            PaletteAction::ToggleNotes,
        ),
        (
            "Toggle Reading list widget".to_string(),
            String::new(),
            PaletteAction::ToggleFeed,
        ),
    ]
}

/// Render the palette dropdown as a floating Area anchored under `anchor` (the omnibar pill
/// rect). `items` is the already-filtered list. Returns the action of a clicked row, if any.
pub(crate) fn draw_palette_dropdown(
    ui: &mut Ui,
    anchor: Rect,
    items: &[(String, String, PaletteAction)],
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
                                egui::RichText::new(tr!("palette-empty"))
                                    .color(pal.muted)
                                    .size(13.0),
                            );
                        });
                        return;
                    }

                    // Cap the height so the dropdown fits BELOW the omnibar. Without this the full
                    // catalog is ~760px tall — too tall to fit under the bar, so egui shoves the
                    // whole area UP to keep it on-screen, covering the omnibar. Scroll for the rest.
                    let max_h = (ui.ctx().screen_rect().bottom() - anchor.bottom() - 40.0)
                        .clamp(180.0, 460.0);
                    egui::ScrollArea::vertical()
                        .max_height(max_h)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
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
        });

    clicked
}
