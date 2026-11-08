//! The interface language from the system's preferred languages (pt-BR, or else en), as the Windows shell's
//! `system_language` does. Pure; the macOS read of `NSLocale.preferredLanguages` lives in `system.rs`.

use swapdesk_ui::Language;

/// `tags` are BCP 47 tags in the user's order of preference ("pt-BR", "en-US", "pt-PT"). Only the first counts: a user
/// who put English first gets English even with Portuguese further down.
pub fn language_from_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Language {
    match tags.into_iter().next() {
        Some(tag) => {
            let primary = tag.split(['-', '_']).next().unwrap_or("");
            if primary.eq_ignore_ascii_case("pt") { Language::PtBr } else { Language::En }
        }
        None => Language::En,
    }
}

/// `SWAPDESK_LANG=en|pt` overrides the system in development, as on Windows.
pub fn language_override(value: Option<&str>) -> Option<Language> {
    match value {
        Some("en") => Some(Language::En),
        Some("pt") => Some(Language::PtBr),
        _ => None,
    }
}

/// `SWAPDESK_THEME=dark|light` overrides the system appearance in development, as on Windows.
pub fn theme_override(value: Option<&str>) -> Option<bool> {
    match value {
        Some("dark") => Some(true),
        Some("light") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portuguese_first_is_pt_br_and_anything_else_is_english() {
        assert_eq!(language_from_tags(["pt-BR", "en-US"]), Language::PtBr);
        assert_eq!(language_from_tags(["pt-PT"]), Language::PtBr);
        assert_eq!(language_from_tags(["pt_BR"]), Language::PtBr);
        assert_eq!(language_from_tags(["en-US", "pt-BR"]), Language::En);
        assert_eq!(language_from_tags(["fr-FR"]), Language::En);
        assert_eq!(language_from_tags([]), Language::En);
    }

    #[test]
    fn overrides_read_only_the_documented_values() {
        assert_eq!(language_override(Some("en")), Some(Language::En));
        assert_eq!(language_override(Some("pt")), Some(Language::PtBr));
        assert_eq!(language_override(Some("xx")), None);
        assert_eq!(theme_override(Some("dark")), Some(true));
        assert_eq!(theme_override(Some("light")), Some(false));
        assert_eq!(theme_override(None), None);
    }
}
