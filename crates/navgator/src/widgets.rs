//! Reusable custom egui 0.34 widgets for the Studio panel and dashboard.
//!
//! egui has no built-in toggle switch or segmented control, so they are
//! hand-painted here. Everything is themed through colors passed by the
//! caller so the widgets stay agnostic of the active palette.
// Consumed by the Studio panel / dashboard phases; allow until those land.
#![allow(dead_code)]

use egui::{
    epaint::{Mesh, Vertex},
    pos2, vec2, Color32, CornerRadius, Frame, Margin, Pos2, Rect, Response, Sense, Shape, Stroke,
    Ui,
};

/// An iOS-style toggle pill (38x22).
///
/// Allocates a fixed-size clickable area, flips `*on` on click, and animates
/// the white knob sliding between the off (left) and on (right) positions.
/// The track is filled `accent` when on, `track_off` when off.
pub(crate) fn toggle_switch(
    ui: &mut Ui,
    on: &mut bool,
    accent: Color32,
    track_off: Color32,
) -> Response {
    let size = vec2(38.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    // Animate the knob from 0.0 (off) to 1.0 (on).
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.18);

    if ui.is_rect_visible(rect) {
        let track_color = track_off.lerp_to_gamma(accent, t);
        let painter = ui.painter();

        // Rounded track.
        painter.rect_filled(rect, CornerRadius::same(11), track_color);

        // White knob (18px diameter => 9px radius), sliding left -> right.
        let knob_radius = 9.0;
        let knob_x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
        let knob_center = pos2(knob_x, rect.center().y);
        painter.circle_filled(knob_center, knob_radius, Color32::WHITE);
    }

    response
}

/// A horizontal segmented control (one button per option).
///
/// The container is a rounded, 1px-bordered track with light padding. The
/// active segment gets an `accent_soft` fill plus a 1px `accent_dim` inset
/// stroke. Sets `*current` to the clicked option and returns `true` if the
/// selection changed.
pub(crate) fn segmented_control<T: PartialEq + Copy>(
    ui: &mut Ui,
    current: &mut T,
    options: &[(T, &str)],
    accent_soft: Color32,
    accent_dim: Color32,
) -> bool {
    let mut changed = false;

    let frame = Frame::NONE
        .stroke(Stroke::new(1.0, accent_dim))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(4));

    frame.show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.horizontal(|ui| {
            for (value, label) in options {
                let selected = *value == *current;
                let fill = if selected {
                    accent_soft
                } else {
                    Color32::TRANSPARENT
                };
                let stroke = if selected {
                    Stroke::new(1.0, accent_dim)
                } else {
                    Stroke::NONE
                };

                let button = egui::Button::new(*label)
                    .fill(fill)
                    .stroke(stroke)
                    .corner_radius(CornerRadius::same(6))
                    .frame(true);

                if ui.add(button).clicked() && !selected {
                    *current = *value;
                    changed = true;
                }
            }
        });
    });

    changed
}

/// A preset gradient swatch: a rounded square filled `c2` with a smaller
/// glowing circle of `c1` centered slightly above-left. Hover-only sensing.
pub(crate) fn gradient_swatch(ui: &mut Ui, size: f32, c1: Color32, c2: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let radius = (size * 0.28).round().clamp(0.0, 127.0) as u8;
        painter.rect_filled(rect, CornerRadius::same(radius), c2);

        let circle_radius = size * 0.78 * 0.5;
        // Offset the glow circle slightly toward the top-left.
        let center = rect.center() - vec2(size * 0.06, size * 0.06);
        painter.circle_filled(center, circle_radius, c1);
    }

    response
}

/// Paint a 135-degree (top-left -> bottom-right) linear gradient into `rect`
/// as a subdivided mesh. `corner` is accepted for API symmetry; rounded
/// corners are approximated as square (see module risks).
pub(crate) fn gradient_avatar(
    painter: &egui::Painter,
    rect: Rect,
    from: Color32,
    to: Color32,
    _corner: f32,
) {
    const N: usize = 8; // grid cells per side
    let mut mesh = Mesh::default();

    // Build (N+1) x (N+1) vertices.
    for iy in 0..=N {
        for ix in 0..=N {
            let fx = ix as f32 / N as f32;
            let fy = iy as f32 / N as f32;
            let pos: Pos2 = pos2(
                rect.left() + fx * rect.width(),
                rect.top() + fy * rect.height(),
            );
            // Diagonal interpolation factor: average of the two fractions.
            let t = ((fx + fy) * 0.5).clamp(0.0, 1.0);
            let color = from.lerp_to_gamma(to, t);
            mesh.vertices.push(Vertex::untextured(pos, color));
        }
    }

    // Two triangles per cell.
    let stride = (N + 1) as u32;
    for iy in 0..N as u32 {
        for ix in 0..N as u32 {
            let i0 = iy * stride + ix;
            let i1 = i0 + 1;
            let i2 = i0 + stride;
            let i3 = i2 + 1;
            mesh.indices.extend_from_slice(&[i0, i1, i2, i1, i3, i2]);
        }
    }

    painter.add(Shape::mesh(mesh));
}

/// A themed card container: filled background, 1px border, rounded corners,
/// and a comfortable inner margin. Returns the [`Frame`] so the caller drives
/// `.show(ui, |ui| { .. })`.
pub(crate) fn card_frame(theme_bg: Color32, border: Color32, radius: u8) -> Frame {
    Frame::NONE
        .fill(theme_bg)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(12))
}
