//! Custom font installation for the NavGator chrome.
//!
//! Embeds three TTFs at compile time and registers them with egui so the UI
//! can use Space Grotesk / Outfit (proportional) and JetBrains Mono
//! (monospace), while keeping egui's built-in fonts as glyph fallbacks.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Install the NavGator fonts into the given egui context.
///
/// Called once at startup (next to `EguiGlow::new`). Safe to call more than
/// once: it simply rebuilds and re-applies the font definitions.
pub(crate) fn install_fonts(ctx: &egui::Context) {
    // Start from the defaults so egui's built-in fallback fonts (emoji, √, …)
    // remain available for glyph fallback.
    let mut defs = FontDefinitions::default();

    // Embed the TTFs at compile time.
    defs.font_data.insert(
        "grotesk".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/SpaceGrotesk.ttf"
        )))),
    );
    defs.font_data.insert(
        "outfit".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/Outfit.ttf"
        )))),
    );
    defs.font_data.insert(
        "jetbrains".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/JetBrainsMono.ttf"
        )))),
    );

    // Prepend our keys to the built-in families so the defaults remain as
    // fallbacks after ours.
    prepend(&mut defs, FontFamily::Proportional, &["grotesk", "outfit"]);
    prepend(&mut defs, FontFamily::Monospace, &["jetbrains"]);

    // Named families for explicit per-widget selection. Each MUST inherit the default fallback
    // chain (egui's bundled symbol/emoji fonts) so chrome glyphs not in our TTFs — ◀ ▶ ↻ ☰ ✕ ★
    // — still resolve instead of rendering as tofu boxes.
    let prop = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mono = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let with_primary = |primary: &str, base: &[String]| {
        let mut v = vec![primary.to_owned()];
        v.extend(base.iter().filter(|f| f.as_str() != primary).cloned());
        v
    };
    defs.families
        .insert(FontFamily::Name("grotesk".into()), with_primary("grotesk", &prop));
    defs.families
        .insert(FontFamily::Name("outfit".into()), with_primary("outfit", &prop));
    defs.families
        .insert(FontFamily::Name("jetbrains".into()), with_primary("jetbrains", &mono));

    ctx.set_fonts(defs);
}

/// Insert `keys` at the front of `family`'s font list, creating the entry if it
/// does not already exist.
fn prepend(defs: &mut FontDefinitions, family: FontFamily, keys: &[&str]) {
    let entry = defs.families.entry(family).or_default();
    for (i, key) in keys.iter().enumerate() {
        entry.insert(i, (*key).to_owned());
    }
}
