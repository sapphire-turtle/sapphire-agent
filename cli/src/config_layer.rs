//! Layering the workspace-level config under the host-local one.
//!
//! Everything here is a pure function over [`toml::Value`]: the allowlist that
//! bounds what the workspace layer may set, the merge that combines the two
//! layers, and the provenance calculation that lets `verify` say where a value
//! came from. Nothing in this module touches the filesystem — `Config::load_layered`
//! reads the files and calls in.

use std::collections::BTreeMap;

/// TOML key paths the workspace-level config layer is permitted to set.
///
/// An entry authorises that path **and everything beneath it**. A key that must
/// stay host-only therefore may not sit under any entry, which is why the
/// `providers` and `room_profile` entries name their permitted leaves one by one
/// instead of using a bare wildcard — `providers.*` would drag `api_key` in with
/// it.
///
/// `*` matches exactly one path segment.
///
/// These are **TOML** names, which are not always the Rust field names: serde
/// renames `room_profiles` to `room_profile`, `memory_namespaces` to
/// `memory_namespace`, `voice_pipelines` to `voice_pipeline`, and
/// `stt_providers` / `tts_providers` to `stt_provider` / `tts_provider`. Whoever
/// adds voice-identity sharing to this allowlist (issue #173) needs the Rust
/// name translated the same way, or the new entries will silently miss every
/// leaf. The fixture test in this module exists to catch a path written in the
/// wrong namespace.
pub const WORKSPACE_ALLOWLIST: &[&[&str]] = &[
    // The agent's identity and model choice.
    &["anthropic", "model"],
    &["anthropic", "light_model"],
    &["anthropic", "max_tokens"],
    &["anthropic", "system_prompt"],
    // Behaviour.
    &["compression"],
    &["day_boundary_hour"],
    &["session_policy"],
    &["daily_log_enabled"],
    &["memory_compaction_enabled"],
    &["heartbeat_enabled"],
    // The autonomous loop's switch and cadence. Allowed for the same
    // reason `heartbeat_enabled` is: it describes how the agent behaves,
    // not where its credentials live. The cost of allowing it is that
    // whoever can write the workspace can set `enabled = true` and
    // `idle_minutes = 0` — the same power `heartbeat_enabled` already
    // hands them, so not a new hole (design doc, risk 5).
    &["autonomous"],
    &["sync_interval_minutes"],
    &["digest"],
    &["timer"],
    // Routing and namespaces.
    &["profiles"],
    &["memory_namespace"],
    &["room_profile", "*", "profile"],
    &["room_profile", "*", "memory_namespace"],
    &["room_profile", "*", "rooms"],
    // Routing, like `rooms` above — which device runs under which profile. Not
    // a credential: the tokens belong to the bridge's external devices, so a
    // poisoned workspace layer can re-route a device but cannot admit one.
    &["room_profile", "*", "devices"],
    &["room_profile", "*", "session_policy"],
    &["room_profile", "*", "voice_pipeline"],
    // Provider refinements. `api_key` and `base_url` are deliberately absent:
    // the endpoint a provider talks to is the one thing that turns a shareable
    // config into remote code execution, because a redirected provider's
    // responses drive tool calls and the tool set includes `shell`. The host
    // decides where a provider lives; the workspace may only refine what it
    // does there.
    &["providers", "*", "type"],
    &["providers", "*", "model"],
    &["providers", "*", "provider_name"],
    &["providers", "*", "max_tokens"],
    // Which named STT/TTS provider a pipeline uses. The providers themselves are
    // host-only because they name model files on disk (see issue #173).
    &["voice_pipeline"],
];

/// True when the workspace layer may set `path`.
///
/// `path` is a TOML key path split into segments. A path is authorised when some
/// allowlist entry is a segment-wise prefix of it.
pub fn path_allowed(path: &[&str]) -> bool {
    WORKSPACE_ALLOWLIST.iter().any(|entry| {
        entry.len() <= path.len()
            && entry
                .iter()
                .zip(path.iter())
                .all(|(entry_seg, path_seg)| *entry_seg == "*" || entry_seg == path_seg)
    })
}

/// Drop every leaf of `workspace` the allowlist does not authorise.
///
/// Returns the filtered document and the sorted dot-joined paths that were
/// dropped, so the caller can name them all in one warning.
///
/// A "leaf" is any value that is not a table — arrays included, because arrays
/// are replaced wholesale rather than merged, so they are authorised or rejected
/// as a unit. Tables left empty by filtering are pruned; an empty table would
/// contribute nothing to the merge anyway.
pub fn filter_allowed(workspace: toml::Value) -> (toml::Value, Vec<String>) {
    let mut rejected = Vec::new();
    let mut trail = Vec::new();
    let kept = filter_inner(workspace, &mut trail, &mut rejected)
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    rejected.sort();
    (kept, rejected)
}

/// Returns `None` when the value was dropped entirely.
fn filter_inner(
    value: toml::Value,
    trail: &mut Vec<String>,
    rejected: &mut Vec<String>,
) -> Option<toml::Value> {
    match value {
        toml::Value::Table(table) => {
            let mut kept = toml::map::Map::new();
            for (key, child) in table {
                trail.push(key.clone());
                if let Some(child) = filter_inner(child, trail, rejected) {
                    kept.insert(key, child);
                }
                trail.pop();
            }
            if kept.is_empty() {
                None
            } else {
                Some(toml::Value::Table(kept))
            }
        }
        leaf => {
            let segments: Vec<&str> = trail.iter().map(String::as_str).collect();
            if path_allowed(&segments) {
                Some(leaf)
            } else {
                rejected.push(trail.join("."));
                None
            }
        }
    }
}

/// Merge `over` onto `base`, with `over` winning.
///
/// Tables merge key by key, recursively. Every other value — scalars and arrays
/// alike — is replaced wholesale by `over`. Arrays are deliberately not
/// concatenated: concatenation cannot express removing an entry, and it silently
/// duplicates values both layers list.
pub fn deep_merge(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(mut base_table), toml::Value::Table(over_table)) => {
            for (key, over_child) in over_table {
                let merged = match base_table.remove(&key) {
                    Some(base_child) => deep_merge(base_child, over_child),
                    None => over_child,
                };
                base_table.insert(key, merged);
            }
            toml::Value::Table(base_table)
        }
        (_, over) => over,
    }
}

/// Which layer supplied the effective value for a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// The workspace-level config, shared by every host using this workspace.
    Workspace,
    /// The host-local config.
    Host,
}

/// The result of layering the workspace config under the host config.
pub struct MergeOutcome {
    /// The merged document, ready to deserialize into `Config`.
    pub merged: toml::Value,
    /// Dot-joined paths dropped from the workspace layer because the allowlist
    /// does not authorise them. Sorted.
    pub rejected: Vec<String>,
    /// Which layer supplied each leaf. See [`provenance_of`].
    pub provenance: BTreeMap<String, Layer>,
}

/// Filter the workspace layer, merge the host over it, and record where each
/// value came from.
pub fn merge_layers(workspace: toml::Value, host: toml::Value) -> MergeOutcome {
    let (workspace, rejected) = filter_allowed(workspace);
    let provenance = provenance_of(&workspace, &host);
    let merged = deep_merge(workspace, host);
    MergeOutcome {
        merged,
        rejected,
        provenance,
    }
}

/// Which layer supplied each leaf, keyed by dot-joined TOML path.
///
/// Only leaves present in one of the two layers appear. A setting absent from
/// both takes its serde default, and callers report that by finding no entry.
///
/// `workspace` is expected to be the **filtered** document, so a rejected key
/// is never attributed to the workspace layer.
///
/// The map keys are the **union of both layers' leaves**, not the leaves of
/// the merged document: a workspace sub-leaf that a host scalar discards
/// wholesale during `deep_merge` (a host `digest = 7` replacing a workspace
/// `[digest]` table, say) can still appear here, attributed to whichever layer
/// actually won. That is latent today because `verify` only ever queries a
/// handful of fixed paths, but a caller that iterates the whole map — as
/// `verify`'s workspace-supplied-settings listing does — will see it.
pub fn provenance_of(workspace: &toml::Value, host: &toml::Value) -> BTreeMap<String, Layer> {
    let mut workspace_paths = Vec::new();
    leaf_paths(workspace, &mut Vec::new(), &mut workspace_paths);
    let mut host_paths = Vec::new();
    leaf_paths(host, &mut Vec::new(), &mut host_paths);

    let mut out: BTreeMap<String, Layer> = workspace_paths
        .into_iter()
        .map(|path| (path, Layer::Workspace))
        .collect();
    // The host wins in `deep_merge`, so it wins here too.
    for path in host_paths {
        out.insert(path, Layer::Host);
    }
    out
}

/// Which layer supplied the key at `path`, for a path that is not
/// necessarily a leaf.
///
/// [`provenance_of`] records leaves, because a *value* is what `verify`
/// reports a layer for. An unknown key is not always a leaf: serde names
/// the key it could not place, so an entire unrecognised table arrives as
/// `foo` with only `foo.bar` and `foo.baz` recorded, and a key inside an
/// array of tables arrives as `tools.mcp_servers.0.typo` where only the
/// array itself was recorded. Hence the two fallbacks — descendants
/// first, then ancestors.
///
/// `None` when nothing matches, which the caller renders as "the
/// configuration" rather than naming the wrong file.
pub fn layer_of(path: &str, provenance: &BTreeMap<String, Layer>) -> Option<Layer> {
    if let Some(layer) = provenance.get(path) {
        return Some(*layer);
    }

    // Descendants: the host wins wherever both layers set something
    // beneath the path, the same way `deep_merge` resolves the value.
    let prefix = format!("{path}.");
    let mut found = None;
    for (leaf, layer) in provenance.range(prefix.clone()..) {
        if !leaf.starts_with(&prefix) {
            break;
        }
        match layer {
            Layer::Host => return Some(Layer::Host),
            Layer::Workspace => found = Some(Layer::Workspace),
        }
    }
    if found.is_some() {
        return found;
    }

    // Ancestors, nearest first: the array or table the key sits inside.
    let mut rest = path;
    while let Some((parent, _)) = rest.rsplit_once('.') {
        if let Some(layer) = provenance.get(parent) {
            return Some(*layer);
        }
        rest = parent;
    }
    None
}

/// Collect the dot-joined path of every leaf (non-table) value.
fn leaf_paths(value: &toml::Value, trail: &mut Vec<String>, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                trail.push(key.clone());
                leaf_paths(child, trail, out);
                trail.pop();
            }
        }
        _ => out.push(trail.join(".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance(entries: &[(&str, Layer)]) -> BTreeMap<String, Layer> {
        entries
            .iter()
            .map(|(path, layer)| ((*path).to_string(), *layer))
            .collect()
    }

    #[test]
    fn layer_of_reads_an_exact_leaf() {
        let p = provenance(&[("room_profile.default.skills", Layer::Host)]);
        assert_eq!(
            layer_of("room_profile.default.skills", &p),
            Some(Layer::Host)
        );
    }

    /// A whole unrecognised table is reported by serde as the table's own
    /// key, which is never a leaf and so is never in `provenance`.
    #[test]
    fn layer_of_falls_back_to_descendants() {
        let p = provenance(&[
            ("typo_table.a", Layer::Workspace),
            ("typo_table.b", Layer::Workspace),
        ]);
        assert_eq!(layer_of("typo_table", &p), Some(Layer::Workspace));
    }

    /// The host wins the merge, so it wins the attribution too — reporting
    /// the workspace file for a key the host also sets would send the
    /// person to a file where deleting it changes nothing.
    #[test]
    fn layer_of_prefers_the_host_among_descendants() {
        let p = provenance(&[
            ("typo_table.a", Layer::Workspace),
            ("typo_table.b", Layer::Host),
        ]);
        assert_eq!(layer_of("typo_table", &p), Some(Layer::Host));
    }

    /// `leaf_paths` stops at an array, so a key inside an array of tables
    /// has no descendant of its own recorded — only the array above it.
    #[test]
    fn layer_of_falls_back_to_an_ancestor() {
        let p = provenance(&[("tools.mcp_servers", Layer::Host)]);
        assert_eq!(layer_of("tools.mcp_servers.0.typo", &p), Some(Layer::Host));
    }

    #[test]
    fn layer_of_is_none_when_nothing_matches() {
        let p = provenance(&[("anthropic.model", Layer::Host)]);
        assert_eq!(layer_of("elsewhere.entirely", &p), None);
    }

    #[test]
    fn exact_leaf_is_allowed() {
        assert!(path_allowed(&["anthropic", "model"]));
        assert!(path_allowed(&["anthropic", "system_prompt"]));
    }

    #[test]
    fn sibling_of_an_allowed_leaf_is_not_allowed() {
        assert!(!path_allowed(&["anthropic", "api_key"]));
    }

    #[test]
    fn an_entry_authorises_everything_beneath_it() {
        assert!(path_allowed(&["timer"]));
        assert!(path_allowed(&["timer", "preset"]));
        assert!(path_allowed(&["timer", "preset", "steps", "label"]));
    }

    #[test]
    fn wildcard_matches_any_single_map_key() {
        assert!(path_allowed(&["room_profile", "work", "profile"]));
        assert!(path_allowed(&["room_profile", "anything-at-all", "rooms"]));
    }

    #[test]
    fn the_workspace_layer_may_route_devices_but_not_hold_tokens() {
        // `devices` is routing, exactly like `rooms`, which is already allowed.
        // `api_keys` held raw credentials and stays refused.
        assert!(path_allowed(&["room_profile", "work", "devices"]));
        assert!(!path_allowed(&["room_profile", "work", "api_keys"]));
    }

    #[test]
    fn wildcard_does_not_authorise_a_host_only_sibling() {
        assert!(!path_allowed(&["room_profile", "work", "api_keys"]));
        assert!(!path_allowed(&["providers", "local", "api_key"]));
        // The endpoint is host-only: a workspace that could redirect a provider
        // could point it at a hostile server, whose responses drive tool calls.
        assert!(!path_allowed(&["providers", "local", "base_url"]));
    }

    #[test]
    fn provider_definition_fields_are_allowed() {
        assert!(path_allowed(&["providers", "local", "type"]));
        assert!(path_allowed(&["providers", "local", "model"]));
        assert!(path_allowed(&["providers", "local", "model"]));
    }

    #[test]
    fn host_only_tables_are_not_allowed() {
        assert!(!path_allowed(&["tools", "tavily_api_key"]));
        assert!(!path_allowed(&["tools", "mcp_servers"]));
        assert!(!path_allowed(&["tools", "admin"]));
        assert!(!path_allowed(&["tools", "admin", "rooms"]));
        assert!(!path_allowed(&["matrix", "access_token"]));
        assert!(!path_allowed(&["serve", "port"]));
        assert!(!path_allowed(&["workspace_dir"]));
        assert!(!path_allowed(&["stt_provider", "whisper", "model_dir"]));
    }

    #[test]
    fn a_table_shorter_than_its_entry_is_not_a_leaf_and_is_not_allowed() {
        // Only leaves are ever tested, so this case cannot arise in practice;
        // pinning it keeps the matcher total.
        assert!(!path_allowed(&["room_profile", "work"]));
    }

    #[test]
    fn acp_is_host_only() {
        // The endpoint a host exposes is a property of the host, not of the
        // shared workspace. The allowlist is default-deny, so this passes
        // without an entry — the test exists to catch someone adding one.
        assert!(!path_allowed(&["acp"]));
        assert!(!path_allowed(&["acp", "enabled"]));
    }

    fn parse(s: &str) -> toml::Value {
        toml::from_str::<toml::Value>(s.trim()).expect("fixture parses")
    }

    #[test]
    fn allowed_leaves_survive_filtering() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
day_boundary_hour = 4

[anthropic]
system_prompt = "you are sapphire"
"#,
        ));
        assert!(rejected.is_empty());
        assert_eq!(kept["day_boundary_hour"].as_integer(), Some(4));
        assert_eq!(
            kept["anthropic"]["system_prompt"].as_str(),
            Some("you are sapphire")
        );
    }

    #[test]
    fn non_allowlisted_leaves_are_dropped_and_reported() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
[anthropic]
model = "claude-opus-5"
api_key = "sk-should-not-travel"

[tools]
tavily_api_key = "also-not"
"#,
        ));
        assert_eq!(
            rejected,
            vec![
                "anthropic.api_key".to_string(),
                "tools.tavily_api_key".to_string()
            ]
        );
        assert_eq!(kept["anthropic"]["model"].as_str(), Some("claude-opus-5"));
        assert!(kept["anthropic"].get("api_key").is_none());
        // `tools` had nothing allowed in it, so the empty table is pruned too.
        assert!(kept.get("tools").is_none());
    }

    #[test]
    fn filtering_is_per_leaf_inside_a_wildcard_table() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
[room_profile.work]
profile = "default"
rooms = ["!a:example.org"]
api_keys = ["sa-secret"]
"#,
        ));
        assert_eq!(rejected, vec!["room_profile.work.api_keys".to_string()]);
        assert_eq!(
            kept["room_profile"]["work"]["profile"].as_str(),
            Some("default")
        );
        assert!(kept["room_profile"]["work"].get("api_keys").is_none());
    }

    #[test]
    fn an_array_is_authorised_as_a_unit() {
        // Arrays are replaced wholesale rather than merged, so they are a leaf.
        let (kept, rejected) = filter_allowed(parse(
            r#"
[[timer.preset]]
name = "pomodoro"

[[tools.mcp_servers]]
name = "evil"
type = "stdio"
command = "rm"
"#,
        ));
        assert_eq!(rejected, vec!["tools.mcp_servers".to_string()]);
        assert!(kept["timer"]["preset"].is_array());
    }

    #[test]
    fn rejected_paths_are_sorted() {
        let (_, rejected) = filter_allowed(parse(
            r#"
workspace_dir = "/tmp/ws"

[serve]
port = 9000

[matrix]
access_token = "t"
"#,
        ));
        assert_eq!(
            rejected,
            vec![
                "matrix.access_token".to_string(),
                "serve.port".to_string(),
                "workspace_dir".to_string()
            ]
        );
    }

    #[test]
    fn host_wins_on_a_scalar() {
        let merged = deep_merge(
            parse("day_boundary_hour = 4"),
            parse("day_boundary_hour = 6"),
        );
        assert_eq!(merged["day_boundary_hour"].as_integer(), Some(6));
    }

    #[test]
    fn workspace_supplies_what_the_host_omits() {
        let merged = deep_merge(
            parse("day_boundary_hour = 4\nsession_policy = \"compact\""),
            parse("day_boundary_hour = 6"),
        );
        assert_eq!(merged["day_boundary_hour"].as_integer(), Some(6));
        assert_eq!(merged["session_policy"].as_str(), Some("compact"));
    }

    #[test]
    fn tables_merge_key_by_key() {
        // The case that forces deep merge: the workspace supplies the shared
        // fields of a room profile, the host adds its bearer token, and neither
        // has to restate the other.
        let merged = deep_merge(
            parse(
                r#"
[room_profile.work]
profile = "default"
rooms = ["!a:example.org"]
"#,
            ),
            parse(
                r#"
[room_profile.work]
api_keys = ["sa-host-only"]
"#,
            ),
        );
        assert_eq!(
            merged["room_profile"]["work"]["profile"].as_str(),
            Some("default")
        );
        assert_eq!(
            merged["room_profile"]["work"]["api_keys"][0].as_str(),
            Some("sa-host-only")
        );
    }

    #[test]
    fn arrays_are_replaced_not_concatenated() {
        // Concatenation cannot express removal, so the host replaces.
        let merged = deep_merge(
            parse(
                r#"[room_profile.work]
rooms = ["!a:example.org", "!b:example.org"]"#,
            ),
            parse(
                r#"[room_profile.work]
rooms = ["!c:example.org"]"#,
            ),
        );
        let rooms = merged["room_profile"]["work"]["rooms"].as_array().unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].as_str(), Some("!c:example.org"));
    }

    #[test]
    fn a_host_scalar_replaces_a_workspace_table() {
        let merged = deep_merge(parse("[digest]\nkeep = 3"), parse("digest = 7"));
        assert_eq!(merged["digest"].as_integer(), Some(7));
    }

    #[test]
    fn provenance_names_the_winning_layer() {
        let outcome = merge_layers(
            parse("day_boundary_hour = 4\nsession_policy = \"compact\""),
            parse("day_boundary_hour = 6"),
        );
        assert_eq!(
            outcome.provenance.get("day_boundary_hour"),
            Some(&Layer::Host)
        );
        assert_eq!(
            outcome.provenance.get("session_policy"),
            Some(&Layer::Workspace)
        );
    }

    #[test]
    fn a_setting_in_neither_layer_has_no_provenance_entry() {
        let outcome = merge_layers(parse(""), parse("day_boundary_hour = 6"));
        assert!(!outcome.provenance.contains_key("heartbeat_enabled"));
    }

    #[test]
    fn a_rejected_key_is_neither_merged_nor_attributed() {
        let outcome = merge_layers(
            parse("[anthropic]\napi_key = \"sk-should-not-travel\""),
            parse("[anthropic]\napi_key = \"sk-host\""),
        );
        assert_eq!(outcome.rejected, vec!["anthropic.api_key".to_string()]);
        assert_eq!(
            outcome.merged["anthropic"]["api_key"].as_str(),
            Some("sk-host")
        );
        assert_eq!(
            outcome.provenance.get("anthropic.api_key"),
            Some(&Layer::Host)
        );
    }

    /// A realistic workspace-level config exercising every allowlist entry.
    ///
    /// Written by hand so the values carry the types the real `Config` expects.
    const FIXTURE: &str = r#"
day_boundary_hour = 4
session_policy = "compact"
daily_log_enabled = true
memory_compaction_enabled = true
heartbeat_enabled = true
sync_interval_minutes = 15

[autonomous]
enabled = true
idle_minutes = 15
origin = "channel"

[anthropic]
model = "claude-opus-5"
light_model = "claude-haiku-4-5-20251001"
max_tokens = 8192
system_prompt = "you are sapphire"

[compression]
enabled = true

[digest]
daily_items = 7

[timer]
server = "timer"

[profiles.default]
provider = "anthropic"
fallback_provider = "local"

[memory_namespace.work]
include = ["default"]
background_profile = "default"

[room_profile.work]
profile = "default"
memory_namespace = "work"
rooms = ["!a:example.org"]
devices = ["a3f9k2p"]
session_policy = "compact"
voice_pipeline = "desk"

[providers.local]
type = "openai_compatible"
model = "qwen"
provider_name = "local"
max_tokens = 4096

[voice_pipeline.desk]
stt_provider = "whisper"
tts_provider = "piper"
"#;

    #[test]
    fn the_fixture_is_entirely_allowlisted() {
        let (_, rejected) = filter_allowed(parse(FIXTURE));
        assert!(
            rejected.is_empty(),
            "fixture has non-allowlisted keys: {rejected:?}"
        );
    }

    #[test]
    fn every_allowlist_entry_is_exercised_by_the_fixture() {
        let mut paths = Vec::new();
        leaf_paths(&parse(FIXTURE), &mut Vec::new(), &mut paths);
        for entry in WORKSPACE_ALLOWLIST {
            let hit = paths.iter().any(|p| {
                let segments: Vec<&str> = p.split('.').collect();
                entry.len() <= segments.len()
                    && entry
                        .iter()
                        .zip(segments.iter())
                        .all(|(e, s)| *e == "*" || e == s)
            });
            assert!(hit, "no fixture key exercises allowlist entry {entry:?}");
        }
    }

    #[test]
    fn every_fixture_key_reaches_a_real_config_field() {
        // The allowlist is string paths, and `Config` does not deny unknown
        // fields, so a path naming a renamed or deleted field would otherwise be
        // ignored in silence. Round-tripping through the type catches it: serde
        // drops an unknown key on the way in, so it is missing on the way out.
        // `providers.local.base_url` is host-only, so the host layer has to
        // supply it for the merged provider to deserialize at all — which is the
        // shape a real deployment takes: the host says where a provider lives,
        // the workspace says what to run there.
        let host = parse(
            r#"
[anthropic]
api_key = "sk-test"

[providers.local]
base_url = "http://llm.lan:8080/v1"
"#,
        );
        let outcome = merge_layers(parse(FIXTURE), host);
        let config: crate::config::Config = outcome
            .merged
            .try_into()
            .expect("merged fixture deserializes");
        let round_tripped = toml::Value::try_from(&config).expect("Config re-serializes");

        let mut paths = Vec::new();
        leaf_paths(&parse(FIXTURE), &mut Vec::new(), &mut paths);
        for path in paths {
            let mut cursor = &round_tripped;
            for segment in path.split('.') {
                cursor = cursor.get(segment).unwrap_or_else(|| {
                    panic!("`{path}` did not survive the round trip through Config")
                });
            }
        }
    }
}
