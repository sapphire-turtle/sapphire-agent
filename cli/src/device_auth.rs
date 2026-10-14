//! Device authentication: bearer token -> external device -> room profile.
//!
//! One mechanism for every authenticated entry point: ambient ingest, `/rpc`,
//! `/a2a` and `/acp`. Tokens belong to the workgroup's external
//! devices, a ledger the sapphire bridge owns and syncs to every host; the
//! agent never sees a stored token. A presented token goes to the bridge
//! (`external_device.authenticate`, for [`APP_NAME`]) through the framework's
//! `BridgeVerifier`, which caches a success for `keys::CACHE_TTL`. When the
//! bridge cannot be asked the request is refused with 503 — closed, never open.
//!
//! Which room profile a device runs under is config data, not ledger data:
//! `[room_profile.<n>].devices`, naming an external device by name or id.
//! That routing array is one of the few settings the workspace layer may
//! also set (`config_layer::WORKSPACE_ALLOWLIST` allowlists
//! `room_profile.*.devices` deliberately) — it is not host-only, just never
//! ledger data.
//!
//! The ledger is synced and can change under a running agent (a device added
//! or retired on another host), so the routing is matched against what the
//! bridge says about the presented token at request time rather than resolved
//! against a snapshot of the ledger at startup.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grain_id::GrainId;
use sapphire_framework::keys::{Authenticated, BridgeVerifier, Verdict, Verifier};

use crate::config::RoomProfileConfig;

/// The application name external devices list in their `apps` to be allowed
/// in here (`sapphire-agent external-device add` fills it in by default).
pub const APP_NAME: &str = "sapphire-agent";

/// What a presented token resolved to.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub device: Authenticated,
    /// Never empty: a device bound to no room profile is refused.
    pub room_profile: String,
}

/// Why a token did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The token is unknown, retired, not allowed to use this app, or its
    /// device is bound to no room profile. Answer 401.
    Refused,
    /// The bridge could not be asked. Answer 503: the token may be fine.
    Unavailable,
}

pub struct DeviceAuth {
    verifier: Arc<dyn Verifier>,
    /// `[room_profile.<n>].devices` entries that name a device -> room profile.
    by_name: HashMap<String, String>,
    /// The entries that read as a grain-id -> room profile.
    by_id: HashMap<GrainId, String>,
}

impl std::fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuth")
            .field("by_name", &self.by_name)
            .field("by_id", &self.by_id)
            .finish_non_exhaustive()
    }
}

impl DeviceAuth {
    /// Authenticate through the host's running bridge.
    pub fn bridge(room_profiles: &HashMap<String, RoomProfileConfig>) -> Result<Self> {
        let verifier = BridgeVerifier::new(APP_NAME, env!("CARGO_PKG_VERSION"));
        Self::new(Arc::new(verifier), room_profiles)
    }

    /// Build the routing from `room_profiles` and check that it is
    /// unambiguous: one device, one room profile.
    ///
    /// Each selector is taken both as a name and, when it reads as one, as a
    /// grain-id — the framework resolves selectors the same way. A selector
    /// listed twice under one room profile is harmless; under two it is an
    /// error, because the room profile is what decides a device's LLM profile
    /// and memory namespace.
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
            by_name,
            by_id,
        })
    }

    /// Device and room profile for `token`.
    ///
    /// Every way a token can be bad collapses to [`AuthError::Refused`] — the
    /// caller answers 401 and the distinction is logged here, not returned.
    pub async fn resolve(&self, token: &str) -> Result<Resolved, AuthError> {
        if token.is_empty() {
            return Err(AuthError::Refused);
        }
        let device = match self.verifier.verify(token).await {
            Verdict::Accepted(device) => device,
            Verdict::Refused => return Err(AuthError::Refused),
            Verdict::Unavailable => {
                tracing::warn!(
                    "device auth: the sapphire bridge could not be asked to check a token; \
                     is it running on this host?"
                );
                return Err(AuthError::Unavailable);
            }
        };
        match self.room_profile_for(&device) {
            Some(room_profile) => Ok(Resolved {
                room_profile: room_profile.to_owned(),
                device,
            }),
            None => {
                tracing::warn!(
                    "device auth: external device '{}' ({}) authenticated but no \
                     [room_profile.<name>].devices lists it",
                    device.name,
                    device.id
                );
                Err(AuthError::Refused)
            }
        }
    }

    /// The room profile `device` runs under. Its name wins over its id, as in
    /// the framework's selector rule.
    fn room_profile_for(&self, device: &Authenticated) -> Option<&str> {
        self.by_name
            .get(&device.name)
            .or_else(|| self.by_id.get(&device.id))
            .map(String::as_str)
    }

    /// The room profile a selector from `[room_profile.<n>].devices` routes to.
    /// For `verify`.
    pub fn room_profile_for_device(&self, id: GrainId, name: &str) -> Option<&str> {
        self.room_profile_for(&Authenticated {
            id,
            name: name.to_owned(),
        })
    }

    /// `(selector, room_profile)` pairs, sorted by selector. For `verify`.
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
/// `DeviceAuth` without a running bridge.
#[cfg(test)]
pub mod testing {
    use super::*;

    pub struct StaticVerifier {
        pub tokens: HashMap<String, Authenticated>,
        pub unavailable: bool,
    }

    #[async_trait::async_trait]
    impl Verifier for StaticVerifier {
        async fn verify(&self, token: &str) -> Verdict {
            if self.unavailable {
                return Verdict::Unavailable;
            }
            match self.tokens.get(token) {
                Some(who) => Verdict::Accepted(who.clone()),
                None => Verdict::Refused,
            }
        }
    }

    /// `(token, device name)` pairs, each device with a fresh id.
    pub fn verifier(devices: &[(&str, &str)]) -> Arc<StaticVerifier> {
        Arc::new(StaticVerifier {
            tokens: devices
                .iter()
                .map(|(token, name)| {
                    (
                        (*token).to_owned(),
                        Authenticated {
                            id: GrainId::random(),
                            name: (*name).to_owned(),
                        },
                    )
                })
                .collect(),
            unavailable: false,
        })
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
    use super::testing::{StaticVerifier, verifier};
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

    #[tokio::test]
    async fn resolves_a_bound_token_by_device_name() {
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
    async fn resolves_a_bound_token_by_device_id() {
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
    async fn refuses_a_token_the_bridge_refuses() {
        let auth = DeviceAuth::new(
            verifier(&[("tok", "pendant")]),
            &rps(&[("pendant", "home")]),
        )
        .unwrap();

        assert_eq!(auth.resolve("nope").await.unwrap_err(), AuthError::Refused);
    }

    #[tokio::test]
    async fn refuses_a_device_bound_to_no_room_profile() {
        let auth =
            DeviceAuth::new(verifier(&[("tok", "stray")]), &rps(&[("pendant", "home")])).unwrap();

        assert_eq!(auth.resolve("tok").await.unwrap_err(), AuthError::Refused);
    }

    #[tokio::test]
    async fn an_unreachable_bridge_is_unavailable_not_refused() {
        let auth = DeviceAuth::new(
            Arc::new(StaticVerifier {
                tokens: HashMap::new(),
                unavailable: true,
            }),
            &rps(&[("pendant", "home")]),
        )
        .unwrap();

        assert_eq!(
            auth.resolve("tok").await.unwrap_err(),
            AuthError::Unavailable
        );
    }

    #[tokio::test]
    async fn refuses_an_empty_token_without_asking() {
        let auth = DeviceAuth::new(
            Arc::new(StaticVerifier {
                tokens: HashMap::new(),
                unavailable: true,
            }),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(auth.resolve("").await.unwrap_err(), AuthError::Refused);
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
