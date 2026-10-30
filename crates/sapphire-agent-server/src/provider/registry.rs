//! Provider registry — bundles all configured `Provider` implementations
//! together with the routing logic that picks one per `room_id`.
//!
//! Constructed once at startup from the parsed `Config`. The built-in
//! Anthropic provider is registered under [`ANTHROPIC_PROVIDER_NAME`] when an
//! API key resolves; additional providers come from `[providers.<name>]`
//! config entries.

use crate::config::{
    ANTHROPIC_PROVIDER_NAME, AnthropicConfig, BACKGROUND_PROFILE_NAME, Config,
    DEFAULT_PROFILE_NAME, ProviderConfig,
};
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
    /// Where a request goes when nothing routes it: a room no profile
    /// claims, a profile name that is not defined, background work with no
    /// background profile. The built-in Anthropic provider when it is
    /// registered, else `[profiles.default]`.
    default: Arc<dyn Provider>,
}

impl ProviderRegistry {
    /// Build a registry from the parsed config.
    ///
    /// Installs the Anthropic provider under `"anthropic"` when its API key
    /// resolves (`[anthropic].api_key` or `ANTHROPIC_API_KEY`). Each
    /// `[providers.<name>]` entry adds another provider keyed by its
    /// configured name. Profile validation is performed up front and
    /// returned as an error so misconfigurations fail at startup rather
    /// than mid-conversation — including a profile that names `anthropic`
    /// when no key resolved, and a deployment with neither a key nor a
    /// `[profiles.default]` to send unrouted requests to.
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::build(config, config.anthropic.resolve_api_key())
    }

    /// [`from_config`](Self::from_config) with the Anthropic key already
    /// resolved, so tests do not depend on the `ANTHROPIC_API_KEY` of
    /// whoever runs them.
    fn build(config: &Config, anthropic_key: Result<String>) -> Result<Self> {
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
        let anthropic_missing = match anthropic_key {
            Ok(key) => {
                let cfg = AnthropicConfig {
                    api_key: Some(key),
                    ..config.anthropic.clone()
                };
                providers.insert(
                    ANTHROPIC_PROVIDER_NAME.to_string(),
                    Arc::new(RetryProvider::new(
                        Arc::new(AnthropicProvider::new(&cfg)?),
                        cfg.incomplete_retries,
                    )),
                );
                None
            }
            Err(e) => Some(e),
        };

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

        if let Some(missing) = anthropic_missing {
            // A profile naming the built-in would otherwise route to a
            // provider that is not there — refuse rather than reroute it.
            let mut users: Vec<String> = config
                .profiles
                .iter()
                .filter(|(_, p)| {
                    p.provider == ANTHROPIC_PROVIDER_NAME
                        || p.fallback_provider.as_deref() == Some(ANTHROPIC_PROVIDER_NAME)
                })
                .map(|(name, _)| format!("[profiles.{name}]"))
                .collect();
            users.sort();
            if !users.is_empty() {
                anyhow::bail!(
                    "{} use(s) the built-in `{ANTHROPIC_PROVIDER_NAME}` provider, but {missing:#}",
                    users.join(", ")
                );
            }
            if !config.profiles.contains_key(DEFAULT_PROFILE_NAME) {
                anyhow::bail!(
                    "{missing:#}. Without the built-in `{ANTHROPIC_PROVIDER_NAME}` provider, \
                     [profiles.{DEFAULT_PROFILE_NAME}] is where unrouted requests go — define it, \
                     naming one of [providers.<name>]"
                );
            }
        }

        let default = match providers.get(ANTHROPIC_PROVIDER_NAME) {
            Some(anthropic) => Arc::clone(anthropic),
            // Validation above guarantees the profile exists and names a
            // registered provider.
            None => resolve_profile(&providers, config, DEFAULT_PROFILE_NAME)
                .expect("[profiles.default] names a registered provider"),
        };

        Ok(Self { providers, default })
    }

    /// Look up a provider by name. Returns `None` if no such provider was
    /// registered. Test-only: routing goes through `resolve_profile`.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }

    /// Where unrouted requests go: the built-in Anthropic provider when it
    /// is registered, else `[profiles.default]`. Always present.
    pub fn default_provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.default)
    }

    /// Resolve the provider that should handle a chat in `room_id`.
    ///
    /// Falls back to [`default_provider`](Self::default_provider) if (a) no profile is configured
    /// for the room, (b) the resolved profile names a provider that isn't
    /// registered (validation should have caught this — defensive only).
    /// When the profile defines a `fallback_provider`, the result is
    /// wrapped in a `FallbackProvider` so refusals or errors transparently
    /// retry on the secondary.
    pub fn for_room(&self, config: &Config, room_id: &str) -> Arc<dyn Provider> {
        let Some(profile_name) = config.profile_for(room_id) else {
            return self.default_provider();
        };
        self.for_profile(config, profile_name)
    }

    /// Resolve the provider for a named profile, honouring its primary +
    /// optional fallback. Falls back to
    /// [`default_provider`](Self::default_provider) if the profile isn't
    /// defined or names an unknown provider.
    ///
    /// The single entry point for profile → provider resolution: room and
    /// session turns go through here, and so do subagent definitions'
    /// `profile:`. The "name not found → Anthropic fallback" behaviour
    /// below is deliberately concentrated here: server configs make it
    /// unreachable by rejecting unknown names at startup
    /// (`Config::validate_subagent_profiles`); a future client/local-loop
    /// config that wants warn-and-fallback instead changes only this point.
    pub fn for_profile(&self, config: &Config, profile_name: &str) -> Arc<dyn Provider> {
        resolve_profile(&self.providers, config, profile_name)
            .unwrap_or_else(|| self.default_provider())
    }

    /// Provider used by background tasks (daily-log, memory compaction,
    /// digests). Resolves the conventional `[profiles.background]`; falls
    /// back to [`default_provider`](Self::default_provider) when that
    /// profile isn't defined.
    pub fn background_provider(&self, config: &Config) -> Arc<dyn Provider> {
        if config.profiles.contains_key(BACKGROUND_PROFILE_NAME) {
            self.for_profile(config, BACKGROUND_PROFILE_NAME)
        } else {
            self.default_provider()
        }
    }

    /// Provider used by background tasks operating under a specific memory
    /// namespace. Resolution order:
    ///
    ///   1. `memory_namespace.<n>.background_profile` if set
    ///   2. global `[profiles.background]` if defined
    ///   3. [`default_provider`](Self::default_provider)
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
            None => self.default_provider(),
        }
    }
}

/// A profile's primary provider, wrapped with its fallback when it names one.
/// `None` when the profile is not defined or its primary is not registered.
fn resolve_profile(
    providers: &HashMap<String, Arc<dyn Provider>>,
    config: &Config,
    profile_name: &str,
) -> Option<Arc<dyn Provider>> {
    let profile = config.profiles.get(profile_name)?;
    let primary = Arc::clone(providers.get(&profile.provider)?);
    match profile
        .fallback_provider
        .as_deref()
        .and_then(|n| providers.get(n))
    {
        Some(fallback) => Some(Arc::new(FallbackProvider::new(
            primary,
            Arc::clone(fallback),
        ))),
        None => Some(primary),
    }
}

/// Registry holding exactly one provider, registered under every name in
/// `names`, and as the default. Test-only: `from_config` builds real
/// providers, so fixtures that want to script replies need a way to install
/// a stub in their place — including as the default, since
/// `ServeState::provider_for_session` falls back to the background provider
/// (which resolves to the default when no `[profiles.background]` is
/// configured).
#[cfg(test)]
impl ProviderRegistry {
    pub(crate) fn for_test(names: &[&str], provider: Arc<dyn Provider>) -> Self {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        for name in names {
            providers.insert(name.to_string(), Arc::clone(&provider));
        }
        Self {
            providers,
            default: provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        toml::from_str(s).expect("config should parse")
    }

    /// Stands in for an `ANTHROPIC_API_KEY`-less environment, whatever the
    /// environment running the tests actually holds.
    fn no_key() -> Result<String> {
        Err(anyhow::anyhow!("no Anthropic API key found"))
    }

    #[test]
    fn without_a_key_the_default_profile_takes_anthropics_place() {
        let cfg = parse(
            r#"
[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "qwen"

[profiles.default]
provider = "local"
"#,
        );
        let reg = ProviderRegistry::build(&cfg, no_key()).unwrap();
        assert!(reg.get(ANTHROPIC_PROVIDER_NAME).is_none());
        assert_eq!(reg.default_provider().name(), "local");
        // Background work with no background profile goes there too.
        assert_eq!(reg.background_provider(&cfg).name(), "local");
        assert_eq!(reg.for_profile(&cfg, "undefined").name(), "local");
    }

    #[test]
    fn without_a_key_a_profile_naming_anthropic_is_refused() {
        let cfg = parse(
            r#"
[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "qwen"

[profiles.default]
provider = "local"

[profiles.smart]
provider = "local"
fallback_provider = "anthropic"
"#,
        );
        let err = ProviderRegistry::build(&cfg, no_key())
            .err()
            .expect("expected an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("[profiles.smart]"), "{msg}");
        assert!(msg.contains("no Anthropic API key found"), "{msg}");
    }

    #[test]
    fn without_a_key_or_a_default_profile_startup_is_refused() {
        let cfg = parse(
            r#"
[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "qwen"
"#,
        );
        let err = ProviderRegistry::build(&cfg, no_key())
            .err()
            .expect("expected an error");
        assert!(format!("{err:#}").contains("[profiles.default]"));
    }

    #[test]
    fn with_a_key_anthropic_stays_the_default_even_beside_a_default_profile() {
        // Unchanged routing for every deployment that has a key: background
        // work without a background profile kept going to Anthropic.
        let cfg = parse(
            r#"
[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "qwen"

[profiles.default]
provider = "local"
"#,
        );
        let reg = ProviderRegistry::build(&cfg, Ok("test".to_string())).unwrap();
        assert_eq!(reg.default_provider().name(), "anthropic");
    }

    #[test]
    fn registry_has_anthropic_when_a_key_resolves() {
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
        let raw_anthropic = reg.get(ANTHROPIC_PROVIDER_NAME).unwrap();
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
        let raw_anthropic = reg.get(ANTHROPIC_PROVIDER_NAME).unwrap();
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
        let raw_anthropic = reg.get(ANTHROPIC_PROVIDER_NAME).unwrap();
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
        let raw_anthropic = reg.get(ANTHROPIC_PROVIDER_NAME).unwrap();
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
