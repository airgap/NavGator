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
    append_system_script_fallbacks(&mut defs);

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

/// System fonts covering scripts our TTFs and egui's bundled fonts lack, so tab titles and page
/// names in Chinese, Japanese, Korean, Arabic and Hebrew show their characters instead of boxes.
/// Too large to embed (Noto CJK is ~20 MB); whichever are installed get appended as the last
/// fallbacks. egui neither shapes nor reorders text, so Arabic and Hebrew stay unjoined and in
/// logical order.
const SCRIPT_FALLBACK_FAMILIES: [&str; 5] = [
    "Noto Sans CJK SC",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Droid Sans Fallback",
    "Noto Sans Devanagari",
];

fn append_system_script_fallbacks(defs: &mut FontDefinitions) {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let mut keys = vec![];
    for family in SCRIPT_FALLBACK_FAMILIES {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..Default::default()
        };
        let Some(id) = database.query(&query) else {
            continue;
        };
        let (data, index) = database
            .with_face_data(id, |data, index| (data.to_vec(), index))
            .expect("A face returned by the query has readable data");
        let mut font_data = FontData::from_owned(data);
        font_data.index = index;
        defs.font_data.insert(family.to_owned(), Arc::new(font_data));
        keys.push(family.to_owned());
    }
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        defs.families.entry(family).or_default().extend(keys.iter().cloned());
    }
}

/// Insert `keys` at the front of `family`'s font list, creating the entry if it
/// does not already exist.
fn prepend(defs: &mut FontDefinitions, family: FontFamily, keys: &[&str]) {
    let entry = defs.families.entry(family).or_default();
    for (i, key) in keys.iter().enumerate() {
        entry.insert(i, (*key).to_owned());
    }
}
