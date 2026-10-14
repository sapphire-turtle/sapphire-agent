//! Which host runs what, once several hosts serve one synced workspace.
//!
//! Every host runs the agent's server, so work that must happen once has to
//! be placed on exactly one of them (design §5,
//! `docs/superpowers/specs/2026-10-13-app-server-sync-acp-stdio-design.md`):
//!
//! - **Chat channels** run where their credentials are. Those are host-local
//!   settings that never sync, and one bot on two hosts would answer every
//!   message twice. Each host publishes what it serves — accounts and rooms,
//!   never a credential — as a claim file in the synced workspace
//!   (`.sapphire-agent/hosts/<device-id>.toml`), so every host can tell which
//!   one serves a room.
//! - **Heartbeats** run on the one host that can deliver them:
//!   [`heartbeat_runner`].
//! - **Background work with no delivery target** — the daily, weekly and
//!   monthly logs, memory compaction, autonomous sessions — runs on the
//!   workspace's primary device, which the framework elects for exactly this
//!   (#182): [`Placement::runs_background`].
//!
//! A host whose workspace is not synced is the only writer there is, and
//! runs everything, as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use sapphire_framework::server::SyncRuntime;
use sapphire_framework_bridge_client::BridgeClient;

/// The directory of claim files, relative to the workspace root.
pub const CLAIMS_DIR: &str = ".sapphire-agent/hosts";

/// A device of the workgroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: GrainId,
    pub name: String,
}

impl Device {
    /// Whether `selector` — a device's name or id, as a heartbeat's
    /// `device:` writes it — names this device. The name wins, as in the
    /// framework's own selector rule.
    pub fn is(&self, selector: &str) -> bool {
        self.name == selector || selector.parse::<GrainId>().is_ok_and(|id| id == self.id)
    }
}

/// What this host is, for placing work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// The workspace is not synced: this host is its only writer.
    Alone,
    /// The workspace is synced with other hosts.
    Member {
        /// This host, once the bridge has said who it is.
        me: Option<Device>,
        /// Whether this host is the workspace's primary device.
        primary: bool,
        /// The workgroup's live devices, this host included.
        peers: Vec<Device>,
    },
}

/// Asks the framework where this host stands.
pub struct Placement {
    synced: Option<Synced>,
}

struct Synced {
    runtime: Arc<SyncRuntime>,
    /// This host's own connection to the bridge, for who is who. Opened on
    /// first use and again after the bridge restarts.
    bridge: tokio::sync::Mutex<Option<Arc<BridgeClient>>>,
    root: PathBuf,
}

impl Synced {
    /// The bridge, connecting to it when there is no live connection.
    async fn bridge(&self) -> Option<Arc<BridgeClient>> {
        let mut bridge = self.bridge.lock().await;
        if let Some(live) = bridge.as_ref().filter(|b| !b.is_closed()) {
            return Some(Arc::clone(live));
        }
        let connected =
            BridgeClient::connect_running(crate::APP_CTX.app_name, crate::app_server::VERSION)
                .await
                .ok()
                .map(Arc::new);
        bridge.clone_from(&connected);
        connected
    }
}

impl Placement {
    /// A host with no sync: it runs everything.
    pub fn alone() -> Arc<Placement> {
        Arc::new(Placement { synced: None })
    }

    /// A host whose workspace `root` may be synced through `runtime`.
    pub fn synced(runtime: Arc<SyncRuntime>, root: PathBuf) -> Arc<Placement> {
        Arc::new(Placement {
            synced: Some(Synced {
                runtime,
                bridge: tokio::sync::Mutex::new(None),
                root,
            }),
        })
    }

    /// This host's role now. Asks the bridge each time: sync can be turned
    /// on or off, and the primary re-elected, while the agent runs.
    pub async fn role(&self) -> Role {
        let Some(s) = &self.synced else {
            return Role::Alone;
        };
        // `status` also refreshes the roles `is_primary` reads.
        let status = s.runtime.status(&s.root).await;
        let (true, Some(workspace_id)) = (status.enabled, status.workspace_id) else {
            return Role::Alone;
        };
        let primary = s.runtime.is_primary(workspace_id).await;
        let bridge = s.bridge().await;
        let answers = match &bridge {
            Some(bridge) => Some((bridge.status().await, bridge.peers().await)),
            None => None,
        };
        let (me, peers) = match answers {
            Some((Ok(st), Ok(p))) => {
                let peers: Vec<Device> = p
                    .peers
                    .iter()
                    .map(|d| Device {
                        id: d.device_id,
                        name: d.name.clone(),
                    })
                    .collect();
                // The bridge names this host by its node id; the device list
                // carries each device's, which is how the two meet.
                let me = p
                    .peers
                    .iter()
                    .find(|d| d.node_id == st.node_id)
                    .map(|d| Device {
                        id: d.device_id,
                        name: d.name.clone(),
                    });
                (me, peers)
            }
            _ => (None, Vec::new()),
        };
        Role::Member { me, primary, peers }
    }

    /// Whether this host runs the work that must happen once and has no
    /// delivery target: alone, or the primary device.
    pub async fn runs_background(&self) -> bool {
        match self.role().await {
            Role::Alone => true,
            Role::Member { primary, .. } => primary,
        }
    }
}

// ---------------------------------------------------------------------------
// Heartbeats
// ---------------------------------------------------------------------------

/// Where a heartbeat says it goes.
#[derive(Clone, Debug, Default)]
pub struct HeartbeatTarget {
    /// `device:` — the device that runs it, by name or id.
    pub device: Option<String>,
    /// `room_id:` — the chat room it is delivered to.
    pub room_id: Option<String>,
    /// `voice.device_id:` — the satellite it is spoken on.
    pub voice: Option<String>,
}

/// What this host knows when a heartbeat fires.
pub struct HostView<'a> {
    pub role: &'a Role,
    /// The rooms this host's channels serve (`seed_routing_from_config`).
    pub local_rooms: &'a HashMap<String, String>,
    /// Whether one of this host's channels lists no rooms — a Discord bot
    /// with no `channel_ids` listens everywhere it can see.
    pub local_catch_all: bool,
    /// The room a heartbeat with no target goes to on this host.
    pub default_room: Option<&'a str>,
    /// Every claim file, this host's own included.
    pub claims: &'a [Claim],
    /// Whether the heartbeat's voice satellite is subscribed to this host.
    pub voice_here: bool,
}

/// Who runs a heartbeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runner {
    /// This host.
    Here,
    /// Another host does; nothing to do here.
    Elsewhere,
    /// No host can. Reported by the primary only, so it is said once.
    Nobody(String),
}

/// The one host that runs `target` (design §5.2).
///
/// | target | runner |
/// |---|---|
/// | `device:` | that device |
/// | `room_id:` | the host that serves the room |
/// | `voice:` only | the host the satellite is subscribed to |
/// | none | the primary, delivering to its default room |
///
/// With both `room_id` and `voice`, the room's host runs it, and its voice
/// fallback stays inside that host: whether the satellite is subscribed to
/// another host is not visible from here, and falling back across hosts
/// could deliver twice.
pub fn heartbeat_runner(target: &HeartbeatTarget, view: &HostView<'_>) -> Runner {
    let Role::Member { me, primary, peers } = view.role else {
        return Runner::Here;
    };
    let primary = *primary;
    if let Some(selector) = &target.device {
        return match me {
            Some(me) if me.is(selector) => Runner::Here,
            Some(_) => Runner::Elsewhere,
            // Who this host is is not known yet; the primary says so.
            None => quiet_unless(primary, "this host's device is not known yet"),
        };
    }
    if let Some(room) = &target.room_id {
        if view.local_rooms.contains_key(room) {
            return Runner::Here;
        }
        let served_elsewhere = view.claims.iter().any(|c| {
            me.as_ref().is_none_or(|m| m.id != c.device_id)
                && peers.iter().any(|p| p.id == c.device_id)
                && c.serves(room)
        });
        return if served_elsewhere {
            Runner::Elsewhere
        } else if view.local_catch_all {
            // No host lists the room, and this one listens everywhere.
            Runner::Here
        } else {
            quiet_unless(primary, &format!("no host serves room {room}"))
        };
    }
    if target.voice.is_some() {
        return if view.voice_here {
            Runner::Here
        } else {
            Runner::Elsewhere
        };
    }
    if !primary {
        return Runner::Elsewhere;
    }
    match view.default_room {
        Some(_) => Runner::Here,
        None => Runner::Nobody(
            "it names no target, and the primary device has no chat channel; give it a \
             `room_id:` or a `device:`"
                .to_owned(),
        ),
    }
}

fn quiet_unless(primary: bool, why: &str) -> Runner {
    if primary {
        Runner::Nobody(why.to_owned())
    } else {
        Runner::Elsewhere
    }
}

// ---------------------------------------------------------------------------
// Claim files
// ---------------------------------------------------------------------------

/// One host's claim: the chat channels it serves. No credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub device_id: GrainId,
    pub device_name: String,
    #[serde(default, rename = "channel")]
    pub channels: Vec<ClaimedChannel>,
}

impl Claim {
    pub fn serves(&self, room: &str) -> bool {
        self.channels
            .iter()
            .any(|c| c.rooms.iter().any(|r| r == room))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedChannel {
    /// `matrix` or `discord`.
    pub kind: String,
    /// Which account: the Matrix user id, or a fingerprint of the Discord
    /// bot token (the token itself never leaves this host's config).
    pub account: String,
    pub rooms: Vec<String>,
}

/// The channels this host's config serves.
pub fn local_channels(config: &crate::config::Config) -> Vec<ClaimedChannel> {
    let mut out = Vec::new();
    if let Some(m) = &config.matrix {
        out.push(ClaimedChannel {
            kind: "matrix".to_owned(),
            account: m.user_id.clone(),
            rooms: m.room_ids.clone(),
        });
    }
    if let Some(d) = &config.discord {
        out.push(ClaimedChannel {
            kind: "discord".to_owned(),
            account: token_fingerprint(&d.bot_token),
            rooms: d.channel_ids.clone(),
        });
    }
    out
}

/// A short, one-way name for a bot token: enough to notice the same bot on
/// two hosts, useless for anything else.
fn token_fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("bot-sha256:{hex}")
}

const CLAIM_HEADER: &str = "\
# Written by sapphire-agent: the chat channels this host serves, so the other
# hosts of the workgroup know where a heartbeat for one of these rooms runs.
# Holds no credentials. Rewritten whenever this host's channels change.
";

fn claim_path(root: &Path, device: GrainId) -> PathBuf {
    root.join(CLAIMS_DIR).join(format!("{device}.toml"))
}

/// Every readable claim file under `root`.
pub fn read_claims(root: &Path) -> Vec<Claim> {
    let Ok(rd) = std::fs::read_dir(root.join(CLAIMS_DIR)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| toml::from_str::<Claim>(&t).map_err(anyhow::Error::from))
        {
            Ok(claim) => out.push(claim),
            Err(e) => warn!("Skipping unreadable host claim {}: {e:#}", path.display()),
        }
    }
    out
}

/// Write (or remove, when it serves nothing) this host's claim. Returns
/// whether the file changed, so a caller rewriting on a timer touches the
/// synced workspace only when something did.
pub fn publish_claim(
    root: &Path,
    me: &Device,
    channels: &[ClaimedChannel],
) -> anyhow::Result<bool> {
    let path = claim_path(root, me.id);
    if channels.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        };
    }
    let claim = Claim {
        device_id: me.id,
        device_name: me.name.clone(),
        channels: channels.to_vec(),
    };
    let body = format!("{CLAIM_HEADER}{}", toml::to_string_pretty(&claim)?);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().expect("a claim has a directory"))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(true)
}

/// Accounts this host serves that another live host claims too: the same
/// bot on two hosts, which answers every message twice.
pub fn shared_accounts<'a>(
    me: &Device,
    mine: &'a [ClaimedChannel],
    claims: &'a [Claim],
    peers: &[Device],
) -> Vec<(&'a ClaimedChannel, &'a Claim)> {
    let mut out = Vec::new();
    for channel in mine {
        for claim in claims {
            if claim.device_id == me.id || !peers.iter().any(|p| p.id == claim.device_id) {
                continue;
            }
            if claim
                .channels
                .iter()
                .any(|c| c.kind == channel.kind && c.account == channel.account)
            {
                out.push((channel, claim));
            }
        }
    }
    out
}

/// Keep this host's claim current, and warn once about a shared account.
///
/// Runs every 30 seconds: the role is only known once the bridge answers,
/// and sync can be enabled after the agent starts.
pub fn spawn_claim_keeper(
    placement: Arc<Placement>,
    root: PathBuf,
    channels: Vec<ClaimedChannel>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut warned = false;
        loop {
            if let Role::Member {
                me: Some(me),
                peers,
                ..
            } = placement.role().await
            {
                if let Err(e) = publish_claim(&root, &me, &channels) {
                    warn!("could not write this host's channel claim: {e:#}");
                }
                let claims = read_claims(&root);
                let shared = shared_accounts(&me, &channels, &claims, &peers);
                if !shared.is_empty() && !warned {
                    for (channel, other) in shared {
                        warn!(
                            "the {} account {} is also configured on the device '{}': both \
                             hosts will answer every message. Keep the credentials on one host.",
                            channel.kind, channel.account, other.device_name
                        );
                    }
                    warned = true;
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> Device {
        Device {
            id: GrainId::random(),
            name: name.to_owned(),
        }
    }

    fn claim(d: &Device, rooms: &[&str]) -> Claim {
        Claim {
            device_id: d.id,
            device_name: d.name.clone(),
            channels: vec![ClaimedChannel {
                kind: "matrix".into(),
                account: format!("@{}:example.org", d.name),
                rooms: rooms.iter().map(|r| (*r).to_owned()).collect(),
            }],
        }
    }

    /// Two hosts: `desk` (primary, serves `!desk`) and `laptop` (serves
    /// `!lap`, and is where the satellite is subscribed). Every target must
    /// be run by exactly one of them.
    struct World {
        desk: Device,
        laptop: Device,
        claims: Vec<Claim>,
    }

    impl World {
        fn new() -> World {
            let desk = dev("desk");
            let laptop = dev("laptop");
            let claims = vec![claim(&desk, &["!desk"]), claim(&laptop, &["!lap"])];
            World {
                desk,
                laptop,
                claims,
            }
        }

        fn runners(&self, target: &HeartbeatTarget) -> Vec<(String, Runner)> {
            let peers = vec![self.desk.clone(), self.laptop.clone()];
            [
                (&self.desk, true, "!desk", false),
                (&self.laptop, false, "!lap", true),
            ]
            .into_iter()
            .map(|(me, primary, room, voice_here)| {
                let role = Role::Member {
                    me: Some(me.clone()),
                    primary,
                    peers: peers.clone(),
                };
                let local: HashMap<String, String> =
                    [(room.to_owned(), "matrix".to_owned())].into();
                let view = HostView {
                    role: &role,
                    local_rooms: &local,
                    local_catch_all: false,
                    default_room: Some(room),
                    claims: &self.claims,
                    voice_here,
                };
                (me.name.clone(), heartbeat_runner(target, &view))
            })
            .collect()
        }

        fn the_one_runner(&self, target: &HeartbeatTarget) -> String {
            let here: Vec<String> = self
                .runners(target)
                .into_iter()
                .filter(|(_, r)| *r == Runner::Here)
                .map(|(n, _)| n)
                .collect();
            assert_eq!(here.len(), 1, "exactly one host runs {target:?}: {here:?}");
            here.into_iter().next().unwrap()
        }
    }

    #[test]
    fn a_named_device_runs_it() {
        let w = World::new();
        let t = HeartbeatTarget {
            device: Some("laptop".into()),
            room_id: Some("!desk".into()),
            ..Default::default()
        };
        assert_eq!(w.the_one_runner(&t), "laptop", "device wins over room");
        let by_id = HeartbeatTarget {
            device: Some(w.desk.id.to_string()),
            ..Default::default()
        };
        assert_eq!(w.the_one_runner(&by_id), "desk");
    }

    #[test]
    fn the_rooms_host_runs_it() {
        let w = World::new();
        for (room, host) in [("!desk", "desk"), ("!lap", "laptop")] {
            let t = HeartbeatTarget {
                room_id: Some(room.into()),
                ..Default::default()
            };
            assert_eq!(w.the_one_runner(&t), host);
        }
    }

    #[test]
    fn with_a_room_and_a_voice_the_rooms_host_runs_it() {
        // The satellite is subscribed to the laptop, but the room is the
        // desk's: the desk runs it and falls back inside itself.
        let w = World::new();
        let t = HeartbeatTarget {
            room_id: Some("!desk".into()),
            voice: Some("sat".into()),
            ..Default::default()
        };
        assert_eq!(w.the_one_runner(&t), "desk");
    }

    #[test]
    fn a_voice_only_heartbeat_runs_where_the_satellite_is() {
        let w = World::new();
        let t = HeartbeatTarget {
            voice: Some("sat".into()),
            ..Default::default()
        };
        assert_eq!(w.the_one_runner(&t), "laptop");
    }

    #[test]
    fn a_heartbeat_with_no_target_runs_on_the_primary() {
        let w = World::new();
        assert_eq!(w.the_one_runner(&HeartbeatTarget::default()), "desk");
    }

    #[test]
    fn a_room_nobody_serves_is_reported_once_by_the_primary() {
        let w = World::new();
        let t = HeartbeatTarget {
            room_id: Some("!gone".into()),
            ..Default::default()
        };
        let runners = w.runners(&t);
        assert!(matches!(runners[0].1, Runner::Nobody(_)), "{runners:?}");
        assert_eq!(runners[1].1, Runner::Elsewhere);
    }

    #[test]
    fn a_retired_hosts_claim_does_not_count() {
        let mut w = World::new();
        let gone = dev("gone");
        w.claims.push(claim(&gone, &["!old"]));
        let t = HeartbeatTarget {
            room_id: Some("!old".into()),
            ..Default::default()
        };
        // `gone` is not among the live peers, so nobody serves `!old`.
        assert!(matches!(w.runners(&t)[0].1, Runner::Nobody(_)));
    }

    #[test]
    fn a_catch_all_channel_takes_rooms_no_host_lists() {
        let w = World::new();
        let peers = vec![w.desk.clone(), w.laptop.clone()];
        let role = Role::Member {
            me: Some(w.laptop.clone()),
            primary: false,
            peers,
        };
        let local = HashMap::new();
        let view = HostView {
            role: &role,
            local_rooms: &local,
            local_catch_all: true,
            default_room: None,
            claims: &w.claims,
            voice_here: false,
        };
        let unlisted = HeartbeatTarget {
            room_id: Some("98765".into()),
            ..Default::default()
        };
        assert_eq!(heartbeat_runner(&unlisted, &view), Runner::Here);
        let desks = HeartbeatTarget {
            room_id: Some("!desk".into()),
            ..Default::default()
        };
        assert_eq!(
            heartbeat_runner(&desks, &view),
            Runner::Elsewhere,
            "an explicit claim wins"
        );
    }

    #[test]
    fn alone_runs_everything() {
        let local = HashMap::new();
        let view = HostView {
            role: &Role::Alone,
            local_rooms: &local,
            local_catch_all: false,
            default_room: None,
            claims: &[],
            voice_here: false,
        };
        for t in [
            HeartbeatTarget::default(),
            HeartbeatTarget {
                room_id: Some("!x".into()),
                ..Default::default()
            },
            HeartbeatTarget {
                device: Some("elsewhere".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(heartbeat_runner(&t, &view), Runner::Here);
        }
    }

    #[test]
    fn a_claim_round_trips_and_holds_no_token() {
        let tmp = tempfile::tempdir().unwrap();
        let me = dev("desk");
        let mut config = crate::config::Config::for_test();
        config.discord = Some(crate::config::DiscordConfig {
            bot_token: "SECRET-TOKEN".into(),
            channel_ids: vec!["123".into()],
            allowed_users: vec![],
        });
        let channels = local_channels(&config);

        assert!(publish_claim(tmp.path(), &me, &channels).unwrap());
        assert!(
            !publish_claim(tmp.path(), &me, &channels).unwrap(),
            "unchanged"
        );

        let raw = std::fs::read_to_string(claim_path(tmp.path(), me.id)).unwrap();
        assert!(!raw.contains("SECRET-TOKEN"), "{raw}");
        let claims = read_claims(tmp.path());
        assert_eq!(claims.len(), 1);
        assert!(claims[0].serves("123"));

        assert!(publish_claim(tmp.path(), &me, &[]).unwrap(), "removed");
        assert!(read_claims(tmp.path()).is_empty());
    }

    #[test]
    fn the_same_account_on_two_live_hosts_is_found() {
        let me = dev("desk");
        let other = dev("laptop");
        let mine = claim(&me, &["!a"]).channels;
        let mut theirs = claim(&other, &["!b"]);
        theirs.channels[0].account = mine[0].account.clone();
        let claims = vec![claim(&me, &["!a"]), theirs];

        let peers = vec![me.clone(), other.clone()];
        assert_eq!(shared_accounts(&me, &mine, &claims, &peers).len(), 1);
        // A retired host's leftover claim is not a conflict.
        assert!(shared_accounts(&me, &mine, &claims, std::slice::from_ref(&me)).is_empty());
    }
}
