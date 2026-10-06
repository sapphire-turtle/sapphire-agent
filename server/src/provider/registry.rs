//! Provider registry — bundles all configured `Provider` implementations
//! together with the routing logic that picks one per `room_id`.
//!
//! Constructed once at startup from the parsed `Config`. The Anthropic
//! provider is always present under the name [`ANTHROPIC_PROVIDER_NAME`];
//! additional providers come from `[providers.<name>]` config entries.

use crate::config::{ANTHROPIC_PROVIDER_NAME, BACKGROUND_PROFILE_NAME, Config, ProviderConfig};
use crate::provider::Provider;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::fallback::FallbackProvider;
use crate::provider::openai_compatible::OpenAICompatibleProvider;
use crate::provider::retry::RetryProvider;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// Build a registry from the parsed config.
    ///
    /// Always installs the Anthropic provider under `"anthropic"`. Each
    /// `[providers.<name>]` entry adds another provider keyed by its
    /// configured name. Profile validation is performed up front and
    /// returned as an error so misconfigurations fail at startup rather
    /// than mid-conversation.
    pub fn from_config(config: &Config) -> Result<Self> {
        let errors = config.validate_profiles();
        if !errors.is_empty() {
            anyhow::bail!(
                "invalid profile configuration:\n  - {}",
                errors.join("\n  - ")
            );
        }

        // Every provider is wrapped in its own `RetryProvider`, beneath any
        // `FallbackProvider` a profile adds: a primary that keeps coming
        // back truncated or empty exhausts its own retries first, and only
        // then does the fallback see it as the error it is.
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert(
            ANTHROPIC_PROVIDER_NAME.to_string(),
            Arc::new(RetryProvider::new(
                Arc::new(AnthropicProvider::new(&config.anthropic)?),
                config.anthropic.incomplete_retries,
            )),
        );

        for (name, pcfg) in &config.providers {
            if name == ANTHROPIC_PROVIDER_NAME {
                anyhow::bail!(
                    "provider name '{ANTHROPIC_PROVIDER_NAME}' is reserved for the built-in"
                );
            }
            let provider: Arc<dyn Provider> = match pcfg {
                ProviderConfig::OpenAiCompatible(c) => {
                    let mut cfg = c.clone();
                    if cfg.provider_name.is_none() {
                        cfg.provider_name = Some(name.clone());
                    }
                    Arc::new(RetryProvider::new(
                        Arc::new(OpenAICompatibleProvider::new(&cfg)),
                        cfg.incomplete_retries,
                    ))
                }
            };
            providers.insert(name.clone(), provider);
        }

        Ok(Self { providers })
    }

    /// Look up a provider by name. Returns `None` if no such provider was
    /// registered.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }

    /// The built-in Anthropic provider — guaranteed to be present.
    pub fn anthropic(&self) -> Arc<dyn Provider> {
        self.providers
            .get(ANTHROPIC_PROVIDER_NAME)
            .cloned()
            .expect("anthropic provider is always registered")
    }

    /// Resolve the provider that should handle a chat in `room_id`.
    ///
    /// Falls back to the Anthropic provider if (a) no profile is configured
    /// for the room, (b) the resolved profile names a provider that isn't
    /// registered (validation should have caught this — defensive only).
    /// When the profile defines a `fallback_provider`, the result is
    /// wrapped in a `FallbackProvider` so refusals or errors transparently
    /// retry on the secondary.
    pub fn for_room(&self, config: &Config, room_id: &str) -> Arc<dyn Provider> {
        let Some(profile_name) = config.profile_for(room_id) else {
            return self.anthropic();
        };
        self.for_profile(config, profile_name)
    }

    /// Resolve the provider for a named profile, honouring its primary +
    /// optional fallback. Falls back to the Anthropic provider if the
    /// profile isn't defined or names an unknown provider.
    ///
    /// The single entry point for profile → provider resolution: room and
    /// session turns go through here, and so do subagent definitions'
    /// `profile:`. The "name not found → Anthropic fallback" behaviour
    /// below is deliberately concentrated here: server configs make it
    /// unreachable by rejecting unknown names at startup
    /// (`Config::validate_subagent_profiles`); a future client/local-loop
    /// config that wants warn-and-fallback instead changes only this point.
    pub fn for_profile(&self, config: &Config, profile_name: &str) -> Arc<dyn Provider> {
        let Some(profile) = config.profiles.get(profile_name) else {
            return self.anthropic();
        };
        let primary = self
            .get(&profile.provider)
            .unwrap_or_else(|| self.anthropic());
        match profile
            .fallback_provider
            .as_deref()
            .and_then(|n| self.get(n))
        {
            Some(fallback) => Arc::new(FallbackProvider::new(primary, fallback)),
            None => primary,
        }
    }

    /// Provider used by background tasks (daily-log, memory compaction,
    /// digests). Resolves the conventional `[profiles.background]`; falls
    /// back to plain Anthropic when that profile isn't defined.
    pub fn background_provider(&self, config: &Config) -> Arc<dyn Provider> {
        if config.profiles.contains_key(BACKGROUND_PROFILE_NAME) {
            self.for_profile(config, BACKGROUND_PROFILE_NAME)
        } else {
            self.anthropic()
        }
    }

    /// Provider used by background tasks operating under a specific memory
    /// namespace. Resolution order:
    ///
    ///   1. `memory_namespace.<n>.background_profile` if set
    ///   2. global `[profiles.background]` if defined
    ///   3. plain Anthropic
    ///
    /// Lets an NSFW namespace route directly to its permissive local
    /// provider without paying a refusal-fallback hop on every daily-log
    /// generation.
    pub fn background_provider_for_namespace(
        &self,
        config: &Config,
        namespace: &str,
    ) -> Arc<dyn Provider> {
        match config.background_profile_for_namespace(namespace) {
            Some(profile_name) => self.for_profile(config, profile_name),
            None => self.anthropic(),
        }
    }
}

/// Registry holding exactly one provider, registered under every name in
/// `names`. Test-only: `from_config` always builds a real Anthropic
/// provider, so fixtures that want to script replies need a way to install
/// a stub in its place — including under the built-in `"anthropic"` key,
/// since `ServeState::provider_for_session` falls back to the background
/// provider (which resolves `"anthropic"` when no `[profiles.background]`
/// is configured).
#[cfg(test)]
impl ProviderRegistry {
    pub(crate) fn for_test(names: &[&str], provider: Arc<dyn Provider>) -> Self {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        for name in names {
            providers.insert(name.to_string(), Arc::clone(&provider));
        }
        Self { providers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        toml::from_str(s).expect("config should parse")
    }

    #[test]
    fn registry_always_has_anthropic() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        assert!(reg.get(ANTHROPIC_PROVIDER_NAME).is_some());
    }

    #[test]
    fn registry_loads_openai_compatible() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        let local = reg.get("local").expect("local provider registered");
        // The provider's name should default to its registry key.
        assert_eq!(local.name(), "local");
    }

    #[test]
    fn from_config_rejects_invalid_profiles() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[profiles.default]
provider = "ghost"
"#,
        );
        let err = ProviderRegistry::from_config(&cfg)
            .err()
            .expect("expected an error");
        assert!(format!("{err:#}").contains("ghost"));
    }

    #[test]
    fn from_config_rejects_reserved_name() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.anthropic]
type = "openai_compatible"
base_url = "http://x/v1"
model = "y"
"#,
        );
        let err = ProviderRegistry::from_config(&cfg)
            .err()
            .expect("expected an error");
        assert!(format!("{err:#}").contains("reserved"));
    }

    #[test]
    fn for_room_falls_back_to_anthropic() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        let p = reg.for_room(&cfg, "!any:srv");
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn for_room_wraps_in_fallback_when_profile_defines_one() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"

[profiles.default]
provider          = "anthropic"
fallback_provider = "local"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        // FallbackProvider::name() forwards to the primary, so the name
        // alone doesn't prove wrapping. Two registry pointers differ in
        // identity, though: the wrapped Arc is freshly allocated.
        let raw_anthropic = reg.anthropic();
        let routed = reg.for_room(&cfg, "!any:srv");
        assert_eq!(routed.name(), "anthropic");
        assert!(
            !Arc::ptr_eq(&raw_anthropic, &routed),
            "expected a wrapped FallbackProvider, got the bare anthropic Arc"
        );
    }

    #[test]
    fn background_provider_uses_background_profile_when_present() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"

[profiles.background]
provider          = "anthropic"
fallback_provider = "local"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        let raw_anthropic = reg.anthropic();
        let bg = reg.background_provider(&cfg);
        assert!(
            !Arc::ptr_eq(&raw_anthropic, &bg),
            "background provider should be wrapped when profile has fallback"
        );
    }

    #[test]
    fn background_provider_for_namespace_uses_namespace_override() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"

[profiles.local_only]
provider = "local"

[memory_namespace.user_nsfw]
include            = ["default"]
background_profile = "local_only"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        let p = reg.background_provider_for_namespace(&cfg, "user_nsfw");
        assert_eq!(p.name(), "local");
        // Namespaces without their own override fall back to anthropic
        // when no global [profiles.background] is defined.
        let p_default = reg.background_provider_for_namespace(&cfg, "default");
        assert_eq!(p_default.name(), "anthropic");
    }

    #[test]
    fn background_provider_for_namespace_falls_back_to_global() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"

[profiles.background]
provider          = "anthropic"
fallback_provider = "local"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        // No namespace override → use the global [profiles.background].
        // The provider's name forwards to the primary (anthropic), but a
        // FallbackProvider was wrapped around it — verify by Arc identity
        // against the bare anthropic provider.
        let raw_anthropic = reg.anthropic();
        let p = reg.background_provider_for_namespace(&cfg, "default");
        assert!(
            !Arc::ptr_eq(&raw_anthropic, &p),
            "expected wrapped FallbackProvider, got the bare anthropic Arc"
        );
    }

    #[test]
    fn background_provider_defaults_to_anthropic_when_not_configured() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        let raw_anthropic = reg.anthropic();
        let bg = reg.background_provider(&cfg);
        assert!(
            Arc::ptr_eq(&raw_anthropic, &bg),
            "without [profiles.background], background should be plain anthropic"
        );
    }

    #[test]
    fn for_room_routes_to_configured_profile() {
        let cfg = parse(
            r#"
[anthropic]
api_key = "test"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "gemma-4-31b-it"

[profiles.nsfw]
provider = "local"

[room_profile.private_nsfw]
profile = "nsfw"
rooms   = ["!nsfw:srv"]
"#,
        );
        let reg = ProviderRegistry::from_config(&cfg).unwrap();
        assert_eq!(reg.for_room(&cfg, "!nsfw:srv").name(), "local");
        assert_eq!(reg.for_room(&cfg, "!other:srv").name(), "anthropic");
    }
}
