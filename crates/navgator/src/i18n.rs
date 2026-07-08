//! Localization (i18n) — Fluent catalogs per locale with an en-US fallback.
//!
//! Every user-facing string in the chrome and the gator:// pages is keyed and looked up here via
//! the [`tr!`](crate::tr) macro. Catalogs live in `crates/navgator/locales/<locale>.ftl` and are
//! embedded at build time. The active locale is detected from the OS at startup (`sys-locale`) and
//! can be overridden from gator://settings.

use std::cell::RefCell;
use std::collections::HashMap;

use fluent::{FluentArgs, FluentBundle, FluentResource};
use unic_langid::{LanguageIdentifier, langid};

/// `(locale-tag, endonym, ftl-source)` for every shipped locale. Order = the settings-picker order.
pub const LOCALES: &[(&str, &str, &str)] = &[
    ("en-US", "English (US)", include_str!("../locales/en-US.ftl")),
    ("en-GB", "English (UK)", include_str!("../locales/en-GB.ftl")),
    ("es", "Español", include_str!("../locales/es.ftl")),
    ("fr", "Français", include_str!("../locales/fr.ftl")),
    ("ru", "Русский", include_str!("../locales/ru.ftl")),
    ("zh", "中文", include_str!("../locales/zh.ftl")),
];

/// The source locale — every key exists here, so it's the fallback when another catalog is missing
/// a key or fails to format it.
const FALLBACK: &str = "en-US";

struct I18n {
    current: String,
    bundles: HashMap<String, FluentBundle<FluentResource>>,
}

thread_local! {
    static I18N: RefCell<I18n> = RefCell::new(I18n::load());
}

impl I18n {
    fn load() -> Self {
        let mut bundles = HashMap::new();
        for (tag, _endonym, ftl) in LOCALES {
            let langid: LanguageIdentifier = tag.parse().unwrap_or_else(|_| langid!("en-US"));
            let resource = match FluentResource::try_new(ftl.to_string()) {
                Ok(resource) => resource,
                // A parse error still yields a partial resource; log and use what parsed.
                Err((resource, errors)) => {
                    log::warn!("i18n: parse errors in {tag}.ftl: {errors:?}");
                    resource
                },
            };
            let mut bundle = FluentBundle::new(vec![langid]);
            // Don't wrap interpolated args in Unicode FSI/PDI isolation marks — they'd show up as
            // stray characters in egui labels and HTML.
            bundle.set_use_isolating(false);
            if let Err(errors) = bundle.add_resource(resource) {
                log::warn!("i18n: {tag}.ftl has duplicate/invalid messages: {errors:?}");
            }
            bundles.insert((*tag).to_string(), bundle);
        }
        Self {
            current: FALLBACK.to_string(),
            bundles,
        }
    }

    fn format(&self, key: &str, args: Option<&FluentArgs>) -> String {
        // Try the active locale, then the fallback catalog.
        for tag in [self.current.as_str(), FALLBACK] {
            let Some(bundle) = self.bundles.get(tag) else {
                continue;
            };
            let Some(message) = bundle.get_message(key) else {
                continue;
            };
            let Some(pattern) = message.value() else {
                continue;
            };
            let mut errors = Vec::new();
            let formatted = bundle.format_pattern(pattern, args, &mut errors);
            // On a formatting error in a non-fallback locale, fall through to en-US; in en-US take
            // it anyway (best effort).
            if errors.is_empty() || tag == FALLBACK {
                return formatted.into_owned();
            }
        }
        // Unknown key: surface it verbatim so missing translations are obvious in the UI.
        key.to_string()
    }
}

/// Translate `key` in the active locale (falling back to en-US, then the key itself).
pub fn tr(key: &str) -> String {
    I18N.with(|i18n| i18n.borrow().format(key, None))
}

/// Translate `key` with interpolation arguments (e.g. counts, names).
pub fn tr_args(key: &str, args: &FluentArgs) -> String {
    I18N.with(|i18n| i18n.borrow().format(key, Some(args)))
}

/// The active locale tag (e.g. `"fr"`).
pub fn current_locale() -> String {
    I18N.with(|i18n| i18n.borrow().current.clone())
}

/// Set the active locale. A tag that isn't a shipped locale is ignored.
pub fn set_locale(tag: &str) {
    I18N.with(|i18n| {
        let mut i18n = i18n.borrow_mut();
        if i18n.bundles.contains_key(tag) {
            i18n.current = tag.to_string();
        }
    });
}

/// Map an OS locale string (`"fr-FR"`, `"zh-Hans-CN"`, `"en_GB.UTF-8"`) to the closest shipped
/// locale tag.
pub fn best_match(os_locale: &str) -> &'static str {
    let lower = os_locale.to_ascii_lowercase();
    // English variants that use British-family spelling.
    for prefix in ["en-gb", "en-au", "en-nz", "en-ie", "en-za", "en_gb"] {
        if lower.starts_with(prefix) {
            return "en-GB";
        }
    }
    if lower.starts_with("en") {
        return "en-US";
    }
    match lower.split(['-', '_', '.']).next().unwrap_or("") {
        "es" => "es",
        "fr" => "fr",
        "ru" => "ru",
        "zh" => "zh",
        _ => "en-US",
    }
}

/// Detect the OS locale at startup and activate the closest shipped catalog. `NAVGATOR_LOCALE`
/// (an exact shipped tag like `fr` / `en-GB`, or any OS-style locale) overrides OS detection.
pub fn init_from_system() {
    let requested = std::env::var("NAVGATOR_LOCALE")
        .ok()
        .or_else(sys_locale::get_locale);
    if let Some(locale) = requested {
        // An exact shipped tag wins (so `NAVGATOR_LOCALE=en-GB` selects UK, not the en-US default
        // that `best_match` would pick for a bare "en"); otherwise map to the nearest.
        if LOCALES.iter().any(|(tag, _, _)| *tag == locale) {
            set_locale(&locale);
        } else {
            set_locale(best_match(&locale));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_load_and_translate() {
        set_locale("fr");
        assert_eq!(tr("toolbar-back"), "Précédent");
        set_locale("ru");
        assert_eq!(tr("toolbar-back"), "Назад");
        set_locale("zh");
        assert_eq!(tr("toolbar-settings"), "设置");
        set_locale("en-GB");
        assert_eq!(tr("toolbar-minimize"), "Minimise");
        // A missing key falls back to en-US, then to the key itself.
        set_locale("es");
        assert_eq!(tr("no-such-key-exists"), "no-such-key-exists");
        set_locale("en-US");
        assert_eq!(tr("toolbar-back"), "Back");
    }

    #[test]
    fn best_match_maps_os_locales() {
        assert_eq!(best_match("fr-FR"), "fr");
        assert_eq!(best_match("es_ES.UTF-8"), "es");
        assert_eq!(best_match("en_GB.UTF-8"), "en-GB");
        assert_eq!(best_match("en-US"), "en-US");
        assert_eq!(best_match("zh-Hans-CN"), "zh");
        assert_eq!(best_match("ru"), "ru");
        assert_eq!(best_match("de-DE"), "en-US");
    }
}
