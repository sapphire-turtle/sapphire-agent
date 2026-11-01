//! Device authentication: bearer token -> API key -> room profile.
//!
//! One mechanism for every authenticated entry point: ambient ingest, `/rpc`,
//! `/a2a` and `/acp`. Tokens belong to the API keys of the workspace the agent
//! serves (`<root>/.sapphire-agent/api_keys/`, managed with
//! `sapphire-agent api-key` or Settings › API keys), which sync with the
//! workspace. The agent never sees a stored token: a presented one goes to the
//! framework's `WorkspaceKeyVerifier`, which caches a success for a short while.
//!
//! **A key's profile is its room profile.** The agent offers its
//! `[room_profile.<n>]` names as the key profiles
//! ([`room_key_profiles`]), so `api-key add <name> --profile <room profile>`
//! makes a key that runs under that room profile, and nothing else needs
//! configuring. A key whose profile is a room profile the config no longer has
//! is refused with 403.
//!
//! **Keys from before profiles.** Keys made before sapphire-framework 0.24 —
//! the workgroup's external devices the framework imported into the
//! workspace — carry the framework's migrated profile, `read-only`. For those,
//! and only those, `[room_profile.<n>].devices` (a key by name or id) still
//! decides the room profile, so an existing device keeps working unchanged
//! until it is re-issued with a room profile. A `devices` entry never moves a
//! key that has a room profile of its own.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use grain_id::GrainId;
use sapphire_framework::keys::{
    Access, Authenticated, KeyProfiles, MIGRATED, Profile, ProfileId, Verdict, Verifier,
};
use sapphire_framework::server::api_keys::WorkspaceKeyVerifier;

use crate::config::{Config, RoomProfileConfig};

/// What a presented token resolved to.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub device: Authenticated,
    /// Never empty: a key bound to no room profile is refused.
    pub room_profile: String,
}

/// Why a token did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The token is unknown or retired, or it is a key from before profiles
    /// that no `[room_profile.<n>].devices` lists. Answer 401.
    Refused,
    /// The token's key has a profile this agent does not offer (a room
    /// profile removed from the config, say). Answer 403: the key is fine,
    /// it may not be used here.
    Forbidden,
}

/// The key profiles for `room_profiles`: one per room profile, each allowed
/// to write — a room profile runs chat turns, which change sessions and
/// memory. The framework's migrated `read-only` is resolved too (to read),
/// though not offered for new keys, so keys from before profiles still
/// authenticate and reach the `devices` fallback.
pub fn room_key_profiles(room_profiles: &HashMap<String, RoomProfileConfig>) -> KeyProfiles {
    let names: Arc<BTreeSet<String>> = Arc::new(room_profiles.keys().cloned().collect());
    let listed = Arc::clone(&names);
    KeyProfiles::new(
        move || profiles_of(&listed),
        move |id| resolve_profile(&names, id),
    )
}

/// The key profiles as the config at `config` (default: the host config)
/// names them now, read again when asked after a short while: for the CLI's
/// `api-key add --profile` and the GUI's API keys page, which run apart from
/// the agent and may outlive a config edit. A config that cannot be read
/// offers no profile (the migrated `read-only` is still resolved). The
/// selected workspace's config layer is read once [`init_app_ctx`] has run.
///
/// [`init_app_ctx`]: crate::init_app_ctx
pub fn configured_key_profiles(config: Option<PathBuf>) -> KeyProfiles {
    let names = Arc::new(ConfiguredNames {
        path: config.unwrap_or_else(Config::default_path),
        cached: Mutex::new(None),
    });
    let listed = Arc::clone(&names);
    KeyProfiles::new(
        move || profiles_of(&listed.get()),
        move |id| resolve_profile(&names.get(), id),
    )
}

/// How long [`configured_key_profiles`] trusts what it read: a settings page
/// asks every frame.
const CONFIG_TTL: Duration = Duration::from_secs(2);

struct ConfiguredNames {
    path: PathBuf,
    cached: Mutex<Option<(Instant, BTreeSet<String>)>>,
}

impl ConfiguredNames {
    fn get(&self) -> BTreeSet<String> {
        let mut cached = self.cached.lock().expect("key profile cache");
        if let Some((at, names)) = cached.as_ref()
            && at.elapsed() < CONFIG_TTL
        {
            return names.clone();
        }
        // The selected workspace's config layer, when the app's directories
        // are known (they are not in a unit test).
        let root = crate::APP_CTX
            .try_config_dir()
            .and_then(|_| crate::app_server::selection().ok().flatten())
            .map(|s| s.root);
        let names: BTreeSet<String> = match Config::load_layered_in(&self.path, root.as_deref()) {
            Ok(loaded) => loaded.config.room_profiles.into_keys().collect(),
            Err(err) => {
                tracing::debug!("no room profiles to offer as key profiles: {err:#}");
                BTreeSet::new()
            }
        };
        *cached = Some((Instant::now(), names.clone()));
        names
    }
}

fn profiles_of(names: &BTreeSet<String>) -> Vec<Profile> {
    names
        .iter()
        .filter_map(|name| {
            Some(Profile {
                id: ProfileId::new(name).ok()?,
                label: format!("Room profile {name}"),
            })
        })
        .collect()
}

fn resolve_profile(names: &BTreeSet<String>, id: &ProfileId) -> Option<Access> {
    if names.contains(id.as_str()) {
        Some(Access::Write)
    } else if id.as_str() == MIGRATED {
        Some(Access::Read)
    } else {
        None
    }
}

pub struct DeviceAuth {
    verifier: Arc<dyn Verifier>,
    /// The configured room profiles: a key whose profile is one of them runs
    /// under it.
    room_profiles: BTreeSet<String>,
    /// `[room_profile.<n>].devices` entries that name a key -> room profile.
    /// Only for keys with the migrated profile.
    by_name: HashMap<String, String>,
    /// The entries that read as a grain-id -> room profile.
    by_id: HashMap<GrainId, String>,
}

impl std::fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuth")
            .field("room_profiles", &self.room_profiles)
            .field("by_name", &self.by_name)
            .field("by_id", &self.by_id)
            .finish_non_exhaustive()
    }
}

impl DeviceAuth {
    /// Authenticate against the API keys of the workspace this agent serves.
    pub fn workspace(room_profiles: &HashMap<String, RoomProfileConfig>) -> Result<Self> {
        let verifier = WorkspaceKeyVerifier::for_app(&crate::APP_CTX)
            .profiles(room_key_profiles(room_profiles));
        Self::new(Arc::new(verifier), room_profiles)
    }

    /// Build the routing from `room_profiles` and check that the legacy
    /// `devices` fallback is unambiguous: one key, one room profile.
    ///
    /// Each selector is taken both as a name and, when it reads as one, as a
    /// grain-id (the ids of keys imported from the workgroup's external
    /// devices). A selector listed twice under one room profile is harmless;
    /// under two it is an error, because the room profile is what decides a
    /// device's LLM profile and memory namespace.
    pub fn new(
        verifier: Arc<dyn Verifier>,
        room_profiles: &HashMap<String, RoomProfileConfig>,
    ) -> Result<Self> {
        // Sorted, so a name in an error is the same one on every run rather
        // than whichever `HashMap` order surfaced.
        let mut ordered: Vec<(&String, &RoomProfileConfig)> = room_profiles.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(b.0));

        let mut by_name: HashMap<String, String> = HashMap::new();
        let mut by_id: HashMap<GrainId, String> = HashMap::new();
        for (rp_name, rp) in ordered {
            for selector in &rp.devices {
                let selector = selector.trim();
                if selector.is_empty() {
                    bail!("room_profile '{rp_name}' lists an empty device selector");
                }
                if let Some(prev) = by_name.get(selector)
                    && prev != rp_name
                {
                    bail!(
                        "device {selector:?} appears in room_profiles '{prev}' and '{rp_name}'; \
                         a device runs under exactly one room profile, which is what decides \
                         its LLM profile and memory namespace"
                    );
                }
                by_name.insert(selector.to_owned(), rp_name.clone());
                if let Ok(id) = selector.parse::<GrainId>() {
                    if let Some(prev) = by_id.get(&id)
                        && prev != rp_name
                    {
                        bail!(
                            "device id {id} appears in room_profiles '{prev}' and '{rp_name}'; \
                             a device runs under exactly one room profile"
                        );
                    }
                    by_id.insert(id, rp_name.clone());
                }
            }
        }

        Ok(Self {
            verifier,
            room_profiles: room_profiles.keys().cloned().collect(),
            by_name,
            by_id,
        })
    }

    /// Key and room profile for `token`.
    ///
    /// Every way a token can be bad but [`AuthError::Forbidden`] collapses to
    /// [`AuthError::Refused`] — the caller answers 401 and the distinction is
    /// logged here, not returned.
    pub async fn resolve(&self, token: &str) -> Result<Resolved, AuthError> {
        if token.is_empty() {
            return Err(AuthError::Refused);
        }
        let device = match self.verifier.verify(token).await {
            Verdict::Accepted(device) => device,
            Verdict::Refused => return Err(AuthError::Refused),
            Verdict::Forbidden => {
                tracing::warn!(
                    "device auth: an API key whose profile is no room profile of this agent was \
                     refused; re-issue it with `sapphire-agent api-key add <name> --profile \
                     <room profile>`"
                );
                return Err(AuthError::Forbidden);
            }
        };
        match self.room_profile_for(&device) {
            Some(room_profile) => Ok(Resolved {
                room_profile: room_profile.to_owned(),
                device,
            }),
            None if device.profile.as_str() == MIGRATED => {
                tracing::warn!(
                    "device auth: API key '{}' ({}) is from before key profiles and no \
                     [room_profile.<name>].devices lists it; re-issue it with \
                     `sapphire-agent api-key add <name> --profile <room profile>`",
                    device.name,
                    device.id
                );
                Err(AuthError::Refused)
            }
            None => {
                // The verifier accepts only profiles `room_key_profiles`
                // resolves, so this is a verifier that offers others.
                tracing::warn!(
                    "device auth: API key '{}' ({}) has profile '{}', which is no room profile",
                    device.name,
                    device.id,
                    device.profile
                );
                Err(AuthError::Forbidden)
            }
        }
    }

    /// The room profile `device` runs under: its profile when that is a room
    /// profile; for a key from before profiles, the `devices` entry naming
    /// it, its name winning over its id.
    fn room_profile_for(&self, device: &Authenticated) -> Option<&str> {
        if let Some(rp) = self.room_profiles.get(device.profile.as_str()) {
            return Some(rp);
        }
        if device.profile.as_str() != MIGRATED {
            return None;
        }
        self.legacy_binding(device.id.as_str(), &device.name)
    }

    /// The room profile `[room_profile.<n>].devices` routes the key `id` /
    /// `name` to. For `verify`, and the fallback above.
    pub fn legacy_binding(&self, id: &str, name: &str) -> Option<&str> {
        self.by_name
            .get(name)
            .or_else(|| {
                id.parse::<GrainId>()
                    .ok()
                    .and_then(|id| self.by_id.get(&id))
            })
            .or_else(|| self.by_name.get(id))
            .map(String::as_str)
    }

    /// Whether `profile` is one of the configured room profiles.
    pub fn is_room_profile(&self, profile: &str) -> bool {
        self.room_profiles.contains(profile)
    }

    /// `(selector, room_profile)` pairs of the `devices` fallback, sorted by
    /// selector. For `verify`.
    pub fn bindings(&self) -> Vec<(&str, &str)> {
        let mut rows: Vec<(&str, &str)> = self
            .by_name
            .iter()
            .map(|(s, rp)| (s.as_str(), rp.as_str()))
            .collect();
        rows.sort();
        rows
    }
}

/// A verifier that answers from a fixed table, for tests that need a
/// `DeviceAuth` without a workspace.
#[cfg(test)]
pub mod testing {
    use super::*;
    use sapphire_framework::keys::KeyId;

    pub struct StaticVerifier {
        pub tokens: HashMap<String, Authenticated>,
        /// Answer every token with `Forbidden`.
        pub forbidden: bool,
    }

    #[async_trait::async_trait]
    impl Verifier for StaticVerifier {
        async fn verify(&self, token: &str) -> Verdict {
            if self.forbidden {
                return Verdict::Forbidden;
            }
            match self.tokens.get(token) {
                Some(who) => Verdict::Accepted(who.clone()),
                None => Verdict::Refused,
            }
        }
    }

    /// `(token, key name, profile)` triples, each key with a fresh id.
    pub fn verifier_with_profiles(keys: &[(&str, &str, &str)]) -> Arc<StaticVerifier> {
        Arc::new(StaticVerifier {
            tokens: keys
                .iter()
                .map(|(token, name, profile)| {
                    (
                        (*token).to_owned(),
                        Authenticated {
                            id: KeyId::new(&GrainId::random().to_string()).unwrap(),
                            name: (*name).to_owned(),
                            profile: ProfileId::new(profile).unwrap(),
                            access: if *profile == MIGRATED {
                                Access::Read
                            } else {
                                Access::Write
                            },
                        },
                    )
                })
                .collect(),
            forbidden: false,
        })
    }

    /// `(token, key name)` pairs: keys from before profiles (`read-only`),
    /// routed by `[room_profile.<n>].devices`.
    pub fn verifier(devices: &[(&str, &str)]) -> Arc<StaticVerifier> {
        let keys: Vec<(&str, &str, &str)> = devices
            .iter()
            .map(|(token, name)| (*token, *name, MIGRATED))
            .collect();
        verifier_with_profiles(&keys)
    }

    /// A `DeviceAuth` over `verifier(devices)`.
    pub fn device_auth(
        devices: &[(&str, &str)],
        room_profiles: &HashMap<String, RoomProfileConfig>,
    ) -> Arc<DeviceAuth> {
        Arc::new(DeviceAuth::new(verifier(devices), room_profiles).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{StaticVerifier, verifier, verifier_with_profiles};
    use super::*;

    fn rps(bindings: &[(&str, &str)]) -> HashMap<String, RoomProfileConfig> {
        let mut out: HashMap<String, RoomProfileConfig> = HashMap::new();
        for (selector, rp) in bindings {
            out.entry((*rp).to_string())
                .or_insert_with(|| RoomProfileConfig {
                    profile: "sonnet".into(),
                    ..Default::default()
                })
                .devices
                .push((*selector).to_string());
        }
        out
    }

    fn room(names: &[&str]) -> HashMap<String, RoomProfileConfig> {
        names
            .iter()
            .map(|n| {
                (
                    (*n).to_string(),
                    RoomProfileConfig {
                        profile: "sonnet".into(),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_key_runs_under_its_profile() {
        let auth = DeviceAuth::new(
            verifier_with_profiles(&[("tok", "pendant", "home")]),
            &room(&["home", "work"]),
        )
        .unwrap();

        let resolved = auth.resolve("tok").await.expect("should resolve");

        assert_eq!(resolved.device.name, "pendant");
        assert_eq!(resolved.room_profile, "home");
    }

    #[tokio::test]
    async fn devices_does_not_move_a_key_with_a_room_profile() {
        let mut rp = rps(&[("pendant", "work")]);
        rp.extend(room(&["home"]));
        let auth =
            DeviceAuth::new(verifier_with_profiles(&[("tok", "pendant", "home")]), &rp).unwrap();

        assert_eq!(auth.resolve("tok").await.unwrap().room_profile, "home");
    }

    #[tokio::test]
    async fn resolves_a_migrated_key_by_name() {
        let auth = DeviceAuth::new(
            verifier(&[("tok", "pendant")]),
            &rps(&[("pendant", "home")]),
        )
        .unwrap();

        let resolved = auth.resolve("tok").await.expect("should resolve");

        assert_eq!(resolved.device.name, "pendant");
        assert_eq!(resolved.room_profile, "home");
    }

    #[tokio::test]
    async fn resolves_a_migrated_key_by_id() {
        let v = verifier(&[("tok", "pendant")]);
        let id = v.tokens["tok"].id.to_string();
        let auth = DeviceAuth::new(v, &rps(&[(&id, "home")])).unwrap();

        assert_eq!(auth.resolve("tok").await.unwrap().room_profile, "home");
    }

    #[tokio::test]
    async fn the_name_wins_over_an_id_naming_another_device() {
        let v = verifier(&[("tok", "pendant")]);
        let id = v.tokens["tok"].id.to_string();
        let auth = DeviceAuth::new(v, &rps(&[("pendant", "home"), (&id, "work")])).unwrap();

        assert_eq!(auth.resolve("tok").await.unwrap().room_profile, "home");
    }

    #[tokio::test]
    async fn refuses_a_token_the_verifier_refuses() {
        let auth = DeviceAuth::new(
            verifier(&[("tok", "pendant")]),
            &rps(&[("pendant", "home")]),
        )
        .unwrap();

        assert_eq!(auth.resolve("nope").await.unwrap_err(), AuthError::Refused);
    }

    #[tokio::test]
    async fn refuses_a_migrated_key_bound_to_no_room_profile() {
        let auth =
            DeviceAuth::new(verifier(&[("tok", "stray")]), &rps(&[("pendant", "home")])).unwrap();

        assert_eq!(auth.resolve("tok").await.unwrap_err(), AuthError::Refused);
    }

    #[tokio::test]
    async fn a_profile_the_agent_does_not_offer_is_forbidden() {
        let auth = DeviceAuth::new(
            Arc::new(StaticVerifier {
                tokens: HashMap::new(),
                forbidden: true,
            }),
            &room(&["home"]),
        )
        .unwrap();
        assert_eq!(auth.resolve("tok").await.unwrap_err(), AuthError::Forbidden);

        // A verifier that accepts a profile the agent does not have.
        let auth = DeviceAuth::new(
            verifier_with_profiles(&[("tok", "pendant", "read-write")]),
            &room(&["home"]),
        )
        .unwrap();
        assert_eq!(auth.resolve("tok").await.unwrap_err(), AuthError::Forbidden);
    }

    #[tokio::test]
    async fn refuses_an_empty_token_without_asking() {
        let auth = DeviceAuth::new(
            Arc::new(StaticVerifier {
                tokens: HashMap::new(),
                forbidden: true,
            }),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(auth.resolve("").await.unwrap_err(), AuthError::Refused);
    }

    #[test]
    fn the_key_profiles_are_the_room_profiles() {
        let profiles = room_key_profiles(&room(&["work", "home"]));
        let ids: Vec<String> = profiles
            .list()
            .into_iter()
            .map(|p| p.id.to_string())
            .collect();
        assert_eq!(ids, ["home", "work"]);
        let id = |s: &str| ProfileId::new(s).unwrap();
        assert_eq!(profiles.access(&id("home")), Some(Access::Write));
        assert_eq!(profiles.access(&id(MIGRATED)), Some(Access::Read));
        assert_eq!(profiles.access(&id("read-write")), None);
        assert_eq!(profiles.access(&id("gone")), None);
    }

    #[test]
    fn configured_key_profiles_read_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[room_profile.home]\nprofile = \"sonnet\"\n").unwrap();
        let profiles = configured_key_profiles(Some(path));
        let ids: Vec<String> = profiles
            .list()
            .into_iter()
            .map(|p| p.id.to_string())
            .collect();
        assert_eq!(ids, ["home"]);
    }

    #[test]
    fn new_rejects_a_device_bound_to_two_room_profiles() {
        let err = DeviceAuth::new(
            verifier(&[]),
            &rps(&[("pendant", "home"), ("pendant", "work")]),
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("home") && msg.contains("work"),
            "name both: {msg}"
        );
    }

    #[test]
    fn new_rejects_one_id_spelled_two_ways_in_two_room_profiles() {
        let id = GrainId::random().to_string();
        let err = DeviceAuth::new(
            verifier(&[]),
            &rps(&[(&id, "home"), (&id.to_uppercase(), "work")]),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains(&id), "{err:#}");
    }

    #[test]
    fn a_selector_repeated_within_one_room_profile_is_fine() {
        assert!(
            DeviceAuth::new(
                verifier(&[]),
                &rps(&[("pendant", "home"), ("pendant", "home")])
            )
            .is_ok()
        );
    }
}
