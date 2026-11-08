//! sapphire-agent-gui's own strings, registered with the framework's i18n
//! beside its own. They follow the language the settings hold
//! (`gui.locale`, the system's by default), as the framework's strings do.

use sapphire_framework_gui::i18n::Resources;

/// This crate's strings: registered every frame by [`crate::settings::follow`].
pub(crate) const RESOURCES: Resources = Resources {
    name: "sapphire-agent-gui",
    files: &[
        ("en", include_str!("../locales/en/agent.ftl")),
        ("ja", include_str!("../locales/ja/agent.ftl")),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_egui::egui;
    use sapphire_framework_gui::i18n::check::{key_difference, parse_errors, unknown_ids};
    use sapphire_framework_gui::i18n::{self, Language, LanguageSettings, Message};
    use std::path::Path;

    const HINT: &str = "agent-companion-input-hint";

    #[test]
    fn agent_files_parse() {
        assert_eq!(parse_errors(&RESOURCES), Vec::<String>::new());
    }

    #[test]
    fn agent_en_and_ja_have_the_same_keys() {
        assert_eq!(key_difference(&RESOURCES, "en", "ja"), Vec::<String>::new());
    }

    #[test]
    fn agent_ids_exist() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert_eq!(
            unknown_ids(&RESOURCES, "en", &src, "agent-", &[]),
            Vec::<String>::new()
        );
    }

    fn hint_in(language: Option<&str>) -> String {
        let ctx = egui::Context::default();
        i18n::register(&ctx, RESOURCES);
        if let Some(tag) = language {
            i18n::install(
                &ctx,
                &LanguageSettings {
                    language: Language::Explicit(tag.into()),
                },
            );
        }
        Message::new(HINT).text(&ctx)
    }

    #[test]
    fn the_input_hint_greets_in_the_chosen_language() {
        assert_eq!(hint_in(Some("ja")), "こんにちは");
        assert_eq!(hint_in(Some("en")), "Hello");
    }

    #[test]
    fn the_input_hint_is_english_before_a_language_is_installed() {
        assert_eq!(hint_in(None), "Hello");
    }
}
