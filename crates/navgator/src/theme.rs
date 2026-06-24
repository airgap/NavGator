//! Live-customization theme engine for the NavGator chrome.
//!
//! This module is the single source of truth for the visual identity: base
//! ramps, accent colors, density tokens, and the derived [`egui::Visuals`] /
//! [`egui::Style`]. Everything is hand-rolled OKLCH -> sRGB (no new deps) so
//! the palette stays perceptually even across bases and accents.
// A few enum labels / Preset::swatch are consumed by the Studio phase; allow until then.
#![allow(dead_code)]

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// The neutral base ramp the whole chrome is built on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Base {
    Graphite,
    Obsidian,
    Midnight,
    Slate,
    Paper,
    Cloud,
}

impl Base {
    /// All bases, in menu order.
    pub(crate) const ALL: &'static [Self] = &[
        Self::Graphite,
        Self::Obsidian,
        Self::Midnight,
        Self::Slate,
        Self::Paper,
        Self::Cloud,
    ];

    /// Human-readable label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Graphite => "Graphite",
            Self::Obsidian => "Obsidian",
            Self::Midnight => "Midnight",
            Self::Slate => "Slate",
            Self::Paper => "Paper",
            Self::Cloud => "Cloud",
        }
    }

    /// Stable lowercase serialization key.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Graphite => "graphite",
            Self::Obsidian => "obsidian",
            Self::Midnight => "midnight",
            Self::Slate => "slate",
            Self::Paper => "paper",
            Self::Cloud => "cloud",
        }
    }

    /// Parse a [`Base::key`].
    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| b.key() == s)
    }

    /// Whether this is a light base (drives the accent White/Dark constraint).
    pub(crate) fn is_light(self) -> bool {
        matches!(self, Self::Paper | Self::Cloud)
    }

    /// The six ramp stops as OKLCH `(L, C, H)`: bg, bg2, elev, border, text, muted.
    fn ramp(self) -> [(f32, f32, f32); 6] {
        match self {
            Self::Graphite => [
                (0.17, 0.012, 285.0),
                (0.205, 0.013, 285.0),
                (0.25, 0.014, 285.0),
                (0.32, 0.015, 285.0),
                (0.94, 0.005, 285.0),
                (0.67, 0.012, 285.0),
            ],
            Self::Obsidian => [
                (0.13, 0.006, 280.0),
                (0.165, 0.007, 280.0),
                (0.21, 0.008, 280.0),
                (0.27, 0.01, 280.0),
                (0.93, 0.004, 280.0),
                (0.62, 0.01, 280.0),
            ],
            Self::Midnight => [
                (0.17, 0.03, 265.0),
                (0.205, 0.032, 265.0),
                (0.25, 0.035, 265.0),
                (0.33, 0.04, 265.0),
                (0.94, 0.01, 265.0),
                (0.68, 0.025, 265.0),
            ],
            Self::Slate => [
                (0.19, 0.012, 245.0),
                (0.225, 0.013, 245.0),
                (0.27, 0.014, 245.0),
                (0.34, 0.016, 245.0),
                (0.94, 0.006, 245.0),
                (0.68, 0.014, 245.0),
            ],
            Self::Paper => [
                (0.98, 0.004, 85.0),
                (0.955, 0.006, 85.0),
                (0.915, 0.008, 85.0),
                (0.87, 0.01, 85.0),
                (0.26, 0.012, 85.0),
                (0.54, 0.014, 85.0),
            ],
            Self::Cloud => [
                (0.985, 0.004, 250.0),
                (0.96, 0.006, 250.0),
                (0.92, 0.009, 250.0),
                (0.875, 0.012, 250.0),
                (0.28, 0.014, 250.0),
                (0.55, 0.016, 250.0),
            ],
        }
    }
}

/// The accent hue applied to interactive/selected chrome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Accent {
    Violet,
    Cyan,
    Magenta,
    Lime,
    Amber,
    Blue,
    White,
    Dark,
}

impl Accent {
    /// All accents, in menu order.
    pub(crate) const ALL: &'static [Self] = &[
        Self::Violet,
        Self::Cyan,
        Self::Magenta,
        Self::Lime,
        Self::Amber,
        Self::Blue,
        Self::White,
        Self::Dark,
    ];

    /// Human-readable label. Note `Lime`'s label is intentionally "Green".
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Violet => "Violet",
            Self::Cyan => "Cyan",
            Self::Magenta => "Magenta",
            Self::Lime => "Green",
            Self::Amber => "Amber",
            Self::Blue => "Blue",
            Self::White => "White",
            Self::Dark => "Dark",
        }
    }

    /// Stable lowercase serialization key.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Violet => "violet",
            Self::Cyan => "cyan",
            Self::Magenta => "magenta",
            Self::Lime => "lime",
            Self::Amber => "amber",
            Self::Blue => "blue",
            Self::White => "white",
            Self::Dark => "dark",
        }
    }

    /// Parse an [`Accent::key`].
    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.key() == s)
    }

    /// OKLCH `(L, C, H)` for this accent.
    pub(crate) fn lch(self) -> (f32, f32, f32) {
        match self {
            Self::Violet => (0.64, 0.18, 300.0),
            Self::Cyan => (0.72, 0.13, 200.0),
            Self::Magenta => (0.64, 0.20, 350.0),
            Self::Lime => (0.68, 0.18, 150.0),
            Self::Amber => (0.78, 0.14, 75.0),
            Self::Blue => (0.62, 0.17, 255.0),
            Self::White => (0.97, 0.006, 285.0),
            Self::Dark => (0.32, 0.01, 285.0),
        }
    }
}

/// Spacing density of the chrome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Density {
    Compact,
    Cozy,
}

impl Density {
    pub(crate) const ALL: &'static [Self] = &[Self::Compact, Self::Cozy];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Cozy => "Cozy",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Cozy => "cozy",
        }
    }

    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|d| d.key() == s)
    }
}

/// Chrome typeface family selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FontChoice {
    Grotesk,
    Sans,
    Mono,
}

impl FontChoice {
    pub(crate) const ALL: &'static [Self] = &[Self::Grotesk, Self::Sans, Self::Mono];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Grotesk => "Grotesk",
            Self::Sans => "Sans",
            Self::Mono => "Mono",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Grotesk => "grotesk",
            Self::Sans => "sans",
            Self::Mono => "mono",
        }
    }

    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.key() == s)
    }
}

/// Where the tab strip lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TabPos {
    Top,
    Left,
}

impl TabPos {
    pub(crate) const ALL: &'static [Self] = &[Self::Top, Self::Left];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Left => "Left",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Left => "left",
        }
    }

    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.key() == s)
    }
}

/// Background wallpaper style behind the chrome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wallpaper {
    Aurora,
    Grid,
    Mesh,
    Mono,
}

impl Wallpaper {
    pub(crate) const ALL: &'static [Self] = &[Self::Aurora, Self::Grid, Self::Mesh, Self::Mono];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Aurora => "Aurora",
            Self::Grid => "Grid",
            Self::Mesh => "Mesh",
            Self::Mono => "Mono",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Aurora => "aurora",
            Self::Grid => "grid",
            Self::Mesh => "mesh",
            Self::Mono => "mono",
        }
    }

    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.key() == s)
    }
}

/// How tabs distribute horizontal space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TabFit {
    Fill,
    Fit,
}

impl TabFit {
    pub(crate) const ALL: &'static [Self] = &[Self::Fill, Self::Fit];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Fill => "Fill",
            Self::Fit => "Fit",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
        }
    }

    pub(crate) fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.key() == s)
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// The complete, serializable description of the chrome's look.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Theme {
    pub(crate) base: Base,
    pub(crate) accent: Accent,
    pub(crate) density: Density,
    pub(crate) font: FontChoice,
    pub(crate) tab_pos: TabPos,
    pub(crate) wallpaper: Wallpaper,
    /// Primary corner radius, `0..=30`.
    pub(crate) radius: u8,
    /// Glass/blur strength, `0..=60`.
    pub(crate) glass: u8,
    pub(crate) tab_fit: TabFit,
    /// Maximum tab width in points, `120..=340`.
    pub(crate) tab_max_w: u16,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            base: Base::Cloud,
            accent: Accent::Violet,
            density: Density::Compact,
            font: FontChoice::Grotesk,
            tab_pos: TabPos::Top,
            wallpaper: Wallpaper::Aurora,
            radius: 0,
            glass: 28,
            tab_fit: TabFit::Fill,
            tab_max_w: 230,
        }
    }
}

impl Theme {
    /// Set the base, enforcing the accent White/Dark constraint for the new
    /// base's lightness.
    pub(crate) fn set_base(&mut self, base: Base) {
        self.base = base;
        if base.is_light() && self.accent == Accent::White {
            self.accent = Accent::Dark;
        } else if !base.is_light() && self.accent == Accent::Dark {
            self.accent = Accent::White;
        }
    }

    /// The accents valid for a given base: White is excluded on light bases,
    /// Dark is excluded on dark bases.
    pub(crate) fn accents_for_base(base: Base) -> Vec<Accent> {
        Accent::ALL
            .iter()
            .copied()
            .filter(|a| {
                if base.is_light() {
                    *a != Accent::White
                } else {
                    *a != Accent::Dark
                }
            })
            .collect()
    }

    /// Small corner radius, derived from [`Theme::radius`].
    pub(crate) fn radius_sm(&self) -> u8 {
        (self.radius as f32 * 0.55).round() as u8
    }

    /// Resolve the concrete color palette for this theme.
    pub(crate) fn palette(&self) -> Palette {
        let r = self.base.ramp();
        let (al, ac, ah) = self.accent.lch();
        Palette {
            bg: oklch(r[0].0, r[0].1, r[0].2),
            bg2: oklch(r[1].0, r[1].1, r[1].2),
            elev: oklch(r[2].0, r[2].1, r[2].2),
            border: oklch(r[3].0, r[3].1, r[3].2),
            text: oklch(r[4].0, r[4].1, r[4].2),
            muted: oklch(r[5].0, r[5].1, r[5].2),
            accent: oklch(al, ac, ah),
            accent_soft: oklch_a(al, ac, ah, 0.16),
            accent_dim: oklch_a(al, ac, ah, 0.45),
        }
    }
}

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

/// New-tab dashboard widget visibility toggles.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Modules {
    pub clock: bool,
    pub search: bool,
    pub sites: bool,
    pub notes: bool,
    pub feed: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Self {
            clock: true,
            search: true,
            sites: true,
            notes: true,
            feed: true,
        }
    }
}

// ---------------------------------------------------------------------------
// OKLCH -> sRGB
// ---------------------------------------------------------------------------

fn gamma(x: f32) -> u8 {
    let lin = x.max(0.0);
    let v = if lin <= 0.0031308 {
        12.92 * lin
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Convert OKLCH (lightness, chroma, hue in degrees) to 8-bit sRGB.
fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> (u8, u8, u8) {
    let h = h_deg * std::f32::consts::PI / 180.0;
    let a = c * h.cos();
    let b = c * h.sin();

    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let lr = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3;
    let lg = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3;
    let lb = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    (gamma(lr), gamma(lg), gamma(lb))
}

/// OKLCH to an opaque [`Color32`].
pub(crate) fn oklch(l: f32, c: f32, h: f32) -> Color32 {
    let (r, g, b) = oklch_to_srgb(l, c, h);
    Color32::from_rgb(r, g, b)
}

/// OKLCH to a [`Color32`] with `alpha` in `0..=1` (unmultiplied).
pub(crate) fn oklch_a(l: f32, c: f32, h: f32, alpha: f32) -> Color32 {
    let (r, g, b) = oklch_to_srgb(l, c, h);
    Color32::from_rgba_unmultiplied(r, g, b, (alpha * 255.0).round() as u8)
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Concrete resolved colors for a [`Theme`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Palette {
    pub bg: Color32,
    pub bg2: Color32,
    pub elev: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_dim: Color32,
}

// ---------------------------------------------------------------------------
// Density tokens
// ---------------------------------------------------------------------------

/// Spacing/sizing tokens derived from [`Density`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct DensityTokens {
    pub pad: f32,
    pub gap: f32,
    pub tab_h: f32,
    pub fs: f32,
    pub strip_v: f32,
    pub bar_h: f32,
    pub omni_h: f32,
    pub omni_px: f32,
    pub bar_px: f32,
    pub bar_gap: f32,
}

/// Resolve the [`DensityTokens`] for a given [`Density`].
pub(crate) fn density_tokens(d: Density) -> DensityTokens {
    match d {
        Density::Compact => DensityTokens {
            pad: 9.0,
            gap: 6.0,
            tab_h: 30.0,
            fs: 12.5,
            strip_v: 0.0,
            bar_h: 42.0,
            omni_h: 30.0,
            omni_px: 12.0,
            bar_px: 9.0,
            bar_gap: 7.0,
        },
        Density::Cozy => DensityTokens {
            pad: 13.0,
            gap: 9.0,
            tab_h: 38.0,
            fs: 13.5,
            strip_v: 7.0,
            bar_h: 50.0,
            omni_h: 36.0,
            omni_px: 14.0,
            bar_px: 12.0,
            bar_gap: 9.0,
        },
    }
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// One-click curated theme starting points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Preset {
    Aurora,
    Terminal,
    Halo,
    Noir,
}

impl Preset {
    pub(crate) const ALL: &'static [Self] =
        &[Self::Aurora, Self::Terminal, Self::Halo, Self::Noir];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Aurora => "Aurora",
            Self::Terminal => "Terminal",
            Self::Halo => "Halo",
            Self::Noir => "Noir",
        }
    }

    /// Short descriptor shown under the preset name.
    pub(crate) fn sub_label(self) -> &'static str {
        match self {
            Self::Aurora => "violet · glass",
            Self::Terminal => "green · mono",
            Self::Halo => "cyan · glass",
            Self::Noir => "magenta · slim",
        }
    }

    /// `(accent color, base bg2 color)` for the composed preview swatch.
    pub(crate) fn swatch(self) -> (Color32, Color32) {
        let mut t = Theme::default();
        self.merge_into(&mut t);
        let (al, ac, ah) = t.accent.lch();
        let bg2 = t.base.ramp()[1];
        (oklch(al, ac, ah), oklch(bg2.0, bg2.1, bg2.2))
    }

    /// Apply this preset's fields onto `t`, leaving unrelated fields (e.g.
    /// `tab_max_w`) untouched.
    pub(crate) fn merge_into(self, t: &mut Theme) {
        match self {
            Self::Aurora => {
                t.base = Base::Graphite;
                t.accent = Accent::Violet;
                t.wallpaper = Wallpaper::Aurora;
                t.font = FontChoice::Grotesk;
                t.tab_pos = TabPos::Top;
                t.radius = 14;
                t.density = Density::Cozy;
                t.glass = 30;
            }
            Self::Terminal => {
                t.base = Base::Obsidian;
                t.accent = Accent::Lime;
                t.wallpaper = Wallpaper::Grid;
                t.font = FontChoice::Mono;
                t.tab_pos = TabPos::Left;
                t.radius = 4;
                t.density = Density::Compact;
                t.glass = 8;
            }
            Self::Halo => {
                t.base = Base::Midnight;
                t.accent = Accent::Cyan;
                t.wallpaper = Wallpaper::Mesh;
                t.font = FontChoice::Grotesk;
                t.tab_pos = TabPos::Top;
                t.radius = 22;
                t.density = Density::Cozy;
                t.glass = 54;
            }
            Self::Noir => {
                t.base = Base::Graphite;
                t.accent = Accent::Magenta;
                t.wallpaper = Wallpaper::Mono;
                t.font = FontChoice::Sans;
                t.tab_pos = TabPos::Left;
                t.radius = 10;
                t.density = Density::Cozy;
                t.glass = 18;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// egui Visuals + Style
// ---------------------------------------------------------------------------

/// The egui font family for a [`FontChoice`]. The named families are
/// registered by `fonts.rs`; if absent, egui falls back gracefully.
pub(crate) fn family(font: FontChoice) -> FontFamily {
    match font {
        FontChoice::Grotesk => FontFamily::Name("grotesk".into()),
        FontChoice::Sans => FontFamily::Name("outfit".into()),
        FontChoice::Mono => FontFamily::Name("jetbrains".into()),
    }
}

/// Build the [`egui::Visuals`] for a theme + its resolved palette.
pub(crate) fn build_visuals(theme: &Theme, pal: &Palette) -> Visuals {
    let mut v = if theme.base.is_light() {
        Visuals::light()
    } else {
        Visuals::dark()
    };

    v.window_fill = pal.bg2;
    v.panel_fill = pal.bg2;
    v.extreme_bg_color = pal.bg;
    v.faint_bg_color = pal.elev;
    v.window_stroke = Stroke::new(1.0, pal.border);

    v.widgets.noninteractive.bg_fill = pal.bg2;
    v.widgets.noninteractive.weak_bg_fill = pal.bg2;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, pal.muted);

    v.widgets.inactive.weak_bg_fill = pal.elev;
    v.widgets.inactive.bg_fill = pal.elev;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, pal.text);

    v.widgets.hovered.weak_bg_fill = pal.elev;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, pal.border);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, pal.text);

    v.widgets.active.weak_bg_fill = pal.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0, pal.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, pal.text);

    v.override_text_color = Some(pal.text);
    v.hyperlink_color = pal.accent;
    v.selection.bg_fill = pal.accent_soft;
    v.selection.stroke = Stroke::new(1.0, pal.accent);
    v.text_cursor.stroke = Stroke::new(2.0, pal.accent);

    let r = CornerRadius::same(theme.radius_sm());
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.corner_radius = r;
    v.widgets.open.corner_radius = r;
    v.window_corner_radius = CornerRadius::same(theme.radius.min(14));

    v
}

/// Apply spacing + text styles derived from the theme onto the egui context.
pub(crate) fn apply_style(ctx: &egui::Context, theme: &Theme) {
    let tk = density_tokens(theme.density);
    let fam = family(theme.font);

    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(tk.gap, tk.gap);
        s.spacing.button_padding = egui::vec2(tk.pad * 0.6, tk.pad * 0.4);

        s.text_styles.insert(TextStyle::Body, FontId::new(tk.fs, fam.clone()));
        s.text_styles.insert(TextStyle::Button, FontId::new(tk.fs, fam.clone()));
        s.text_styles
            .insert(TextStyle::Small, FontId::new(tk.fs - 2.0, fam.clone()));
        s.text_styles
            .insert(TextStyle::Heading, FontId::new(tk.fs * 1.6, fam.clone()));
        s.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(tk.fs, FontFamily::Name("jetbrains".into())),
        );
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklch_white_is_whiteish() {
        let c = oklch(1.0, 0.0, 0.0);
        assert!(c.r() >= 250 && c.g() >= 250 && c.b() >= 250, "{c:?}");
    }

    #[test]
    fn oklch_black_is_black() {
        let c = oklch(0.0, 0.0, 123.0);
        assert_eq!((c.r(), c.g(), c.b()), (0, 0, 0));
    }

    #[test]
    fn paper_bg_near_white() {
        let pal = Theme {
            base: Base::Paper,
            ..Theme::default()
        }
        .palette();
        let bg = pal.bg;
        assert!(bg.r() >= 230 && bg.g() >= 230 && bg.b() >= 230, "{bg:?}");
    }

    #[test]
    fn paper_text_is_dark() {
        let pal = Theme {
            base: Base::Paper,
            ..Theme::default()
        }
        .palette();
        let t = pal.text;
        assert!(t.r() <= 110 && t.g() <= 110 && t.b() <= 110, "{t:?}");
    }

    #[test]
    fn radius_sm_rounds() {
        let t = Theme {
            radius: 30,
            ..Theme::default()
        };
        assert_eq!(t.radius_sm(), 17);
        let t0 = Theme {
            radius: 0,
            ..Theme::default()
        };
        assert_eq!(t0.radius_sm(), 0);
    }

    #[test]
    fn default_base_is_cloud() {
        assert_eq!(Theme::default().base, Base::Cloud);
    }

    #[test]
    fn accents_for_cloud() {
        let a = Theme::accents_for_base(Base::Cloud);
        assert!(!a.contains(&Accent::White));
        assert!(a.contains(&Accent::Dark));
    }

    #[test]
    fn set_base_swaps_dark_to_white() {
        let mut t = Theme {
            base: Base::Paper,
            accent: Accent::Dark,
            ..Theme::default()
        };
        t.set_base(Base::Obsidian);
        assert_eq!(t.accent, Accent::White);
    }

    #[test]
    fn enum_keys_roundtrip() {
        for b in Base::ALL {
            assert_eq!(Base::from_key(b.key()), Some(*b));
        }
        for a in Accent::ALL {
            assert_eq!(Accent::from_key(a.key()), Some(*a));
        }
        for d in Density::ALL {
            assert_eq!(Density::from_key(d.key()), Some(*d));
        }
        for f in FontChoice::ALL {
            assert_eq!(FontChoice::from_key(f.key()), Some(*f));
        }
        for p in TabPos::ALL {
            assert_eq!(TabPos::from_key(p.key()), Some(*p));
        }
        for w in Wallpaper::ALL {
            assert_eq!(Wallpaper::from_key(w.key()), Some(*w));
        }
        for f in TabFit::ALL {
            assert_eq!(TabFit::from_key(f.key()), Some(*f));
        }
        assert_eq!(Base::from_key("nope"), None);
    }

    #[test]
    fn lime_label_is_green() {
        assert_eq!(Accent::Lime.label(), "Green");
    }

    #[test]
    fn preset_terminal_fidelity() {
        let mut t = Theme::default();
        Preset::Terminal.merge_into(&mut t);
        assert_eq!(t.base, Base::Obsidian);
        assert_eq!(t.accent, Accent::Lime);
        assert_eq!(t.wallpaper, Wallpaper::Grid);
        assert_eq!(t.font, FontChoice::Mono);
        assert_eq!(t.tab_pos, TabPos::Left);
        assert_eq!(t.radius, 4);
        assert_eq!(t.density, Density::Compact);
        assert_eq!(t.glass, 8);
    }

    #[test]
    fn preset_aurora_fidelity() {
        let mut t = Theme::default();
        Preset::Aurora.merge_into(&mut t);
        assert_eq!(t.base, Base::Graphite);
        assert_eq!(t.accent, Accent::Violet);
        assert_eq!(t.wallpaper, Wallpaper::Aurora);
        assert_eq!(t.font, FontChoice::Grotesk);
        assert_eq!(t.tab_pos, TabPos::Top);
        assert_eq!((t.radius, t.density, t.glass), (14, Density::Cozy, 30));
    }

    #[test]
    fn preset_leaves_tab_max_w_untouched() {
        let mut t = Theme {
            tab_max_w: 305,
            ..Theme::default()
        };
        Preset::Halo.merge_into(&mut t);
        assert_eq!(t.tab_max_w, 305);
    }

    #[test]
    fn density_token_values() {
        let c = density_tokens(Density::Compact);
        assert_eq!((c.pad, c.gap, c.tab_h, c.bar_h), (9.0, 6.0, 30.0, 42.0));
        let z = density_tokens(Density::Cozy);
        assert_eq!((z.pad, z.gap, z.tab_h, z.bar_h), (13.0, 9.0, 38.0, 50.0));
    }

    #[test]
    fn oklch_blue_channel_dominant() {
        let c = oklch(0.62, 0.17, 255.0);
        assert!(c.b() > c.r() && c.b() > c.g(), "expected blue, got {c:?}");
    }

    #[test]
    fn accent_soft_dim_alphas() {
        let pal = Theme::default().palette();
        assert_eq!(pal.accent_soft.a(), 41); // round(0.16 * 255)
        assert_eq!(pal.accent_dim.a(), 115); // round(0.45 * 255)
    }
}
