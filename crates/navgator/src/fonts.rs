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

    // Named families for explicit per-widget selection.
    defs.families.insert(
        FontFamily::Name("grotesk".into()),
        vec!["grotesk".to_owned(), "outfit".to_owned()],
    );
    defs.families.insert(
        FontFamily::Name("outfit".into()),
        vec!["outfit".to_owned(), "grotesk".to_owned()],
    );
    defs.families.insert(
        FontFamily::Name("jetbrains".into()),
        vec!["jetbrains".to_owned()],
    );

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
