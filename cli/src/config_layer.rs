//! Layering the workspace-level config under the host-local one.
//!
//! Everything here is a pure function over [`toml::Value`]: the denylist that
//! keeps credentials out of the workspace layer, the merge that combines the
//! two layers, and the provenance calculation that lets `verify` say where a
//! value came from. Nothing in this module touches the filesystem —
//! `Config::load_layered` reads the files and calls in.

use std::collections::BTreeMap;

/// Key names the workspace layer may never set, wherever they appear.
///
/// The workspace config is shared: everything that describes the agent —
/// providers and their endpoints, profiles, room routing, MCP servers, tool
/// settings — travels with it, and each host only adds its secrets. A key is
/// a secret by its **name**, not its position, so a credential field added
/// later under any table is caught without touching this list, as long as it
/// is spelled like one (see [`CREDENTIAL_SUFFIXES`]). Names are compared
/// case-insensitively, which also covers an MCP server's `env` table
/// (`GITHUB_TOKEN = "…"`).
pub const CREDENTIAL_KEYS: &[&str] = &[
    "api_key",
    // `[room_profile.<n>].api_keys`, the removed raw-token list.
    "api_keys",
    "token",
    "secret",
    "password",
    // `[matrix].recovery_key`, the E2EE recovery key.
    "recovery_key",
];

/// Suffixes that mark a key as a credential: `tools.tavily_api_key`,
/// `matrix.access_token`, `discord.bot_token`, …
///
/// `max_tokens` is not one: the match is on the whole suffix, and `_tokens`
/// is not `_token`.
pub const CREDENTIAL_SUFFIXES: &[&str] = &["_api_key", "_token", "_secret", "_password"];

/// Paths the workspace layer may not set although they are no credential.
/// An entry covers that path and everything beneath it.
///
/// - `workspace_dir` locates the workspace config, so a workspace config
///   naming it is circular — and the path is one machine's, which another
///   host syncing the same workspace does not have.
/// - `tools.host_access` hands the agent this machine's shell and
///   filesystem. Whether a device grants that is the device's own decision,
///   not one a synced file makes for every host at once.
pub const HOST_ONLY_PATHS: &[&[&str]] = &[&["workspace_dir"], &["tools", "host_access"]];

/// True when `key` names a credential. See [`CREDENTIAL_KEYS`].
pub fn is_credential_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    CREDENTIAL_KEYS.contains(&key.as_str())
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
}

/// True when the workspace layer may set `path`.
///
/// `path` is a TOML key path split into segments, its last segment the key
/// being set. Everything is allowed except a credential key (at any depth)
/// and the [`HOST_ONLY_PATHS`].
pub fn path_allowed(path: &[&str]) -> bool {
    if path.last().is_some_and(|key| is_credential_key(key)) {
        return false;
    }
    !HOST_ONLY_PATHS.iter().any(|entry| path.starts_with(entry))
}

/// Drop every leaf of `workspace` that [`path_allowed`] refuses.
///
/// Returns the filtered document and the sorted dot-joined paths that were
/// dropped, so the caller can name them all in one warning.
///
/// A "leaf" is any value that is not a table — arrays included, because arrays
/// are replaced wholesale rather than merged, so a whole array of credentials
/// (`api_keys = […]`) is rejected as a unit. An array of tables is still
/// searched element by element, so the credential inside one entry
/// (`tools.mcp_servers.0.api_key`) is dropped while the entry itself is
/// shared; the element stays in place, even when nothing is left of it, so
/// the indices the warning names match the file. Tables left empty by
/// filtering are pruned; an empty table would contribute nothing to the
/// merge anyway.
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
    if !trail.is_empty() {
        let segments: Vec<&str> = trail.iter().map(String::as_str).collect();
        if !path_allowed(&segments) {
            rejected.push(trail.join("."));
            return None;
        }
    }
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
        toml::Value::Array(items) => Some(toml::Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, item)| match item {
                    toml::Value::Table(table) => {
                        let mut kept = toml::map::Map::new();
                        for (key, child) in table {
                            trail.push(index.to_string());
                            trail.push(key.clone());
                            if let Some(child) = filter_inner(child, trail, rejected) {
                                kept.insert(key, child);
                            }
                            trail.pop();
                            trail.pop();
                        }
                        toml::Value::Table(kept)
                    }
                    other => other,
                })
                .collect(),
        )),
        leaf => Some(leaf),
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
    /// Dot-joined paths dropped from the workspace layer because they name a
    /// credential or a host-only path. Sorted.
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
    fn shared_settings_are_allowed() {
        assert!(path_allowed(&["anthropic", "model"]));
        assert!(path_allowed(&["providers", "local", "base_url"]));
        assert!(path_allowed(&["providers", "local", "model"]));
        assert!(path_allowed(&["providers", "local", "max_tokens"]));
        assert!(path_allowed(&["profiles", "default", "provider"]));
        assert!(path_allowed(&["room_profile", "work", "devices"]));
        assert!(path_allowed(&["tools", "mcp_servers"]));
        assert!(path_allowed(&["tools", "admin", "room_profiles"]));
        assert!(path_allowed(&["stt_provider", "whisper", "model"]));
        assert!(path_allowed(&["matrix", "homeserver"]));
        // The name of an env var holding a key is not the key.
        assert!(path_allowed(&["stt_provider", "whisper", "api_key_env"]));
    }

    #[test]
    fn credentials_are_refused_at_any_depth() {
        assert!(!path_allowed(&["anthropic", "api_key"]));
        assert!(!path_allowed(&["providers", "local", "api_key"]));
        assert!(!path_allowed(&["tools", "tavily_api_key"]));
        assert!(!path_allowed(&["tools", "mcp_servers", "0", "api_key"]));
        assert!(!path_allowed(&["room_profile", "work", "api_keys"]));
        assert!(!path_allowed(&["matrix", "access_token"]));
        assert!(!path_allowed(&["matrix", "recovery_key"]));
        assert!(!path_allowed(&["discord", "bot_token"]));
    }

    #[test]
    fn credential_names_match_case_insensitively() {
        // An MCP server's `env` table is spelled the way env vars are.
        assert!(!path_allowed(&[
            "tools",
            "mcp_servers",
            "0",
            "env",
            "GITHUB_TOKEN"
        ]));
        assert!(path_allowed(&[
            "tools",
            "mcp_servers",
            "0",
            "env",
            "RUST_LOG"
        ]));
    }

    #[test]
    fn host_only_paths_are_refused_with_everything_beneath_them() {
        assert!(!path_allowed(&["workspace_dir"]));
        assert!(!path_allowed(&["tools", "host_access"]));
        assert!(!path_allowed(&["tools", "host_access", "enabled"]));
        // The rest of `[tools]` is shared.
        assert!(path_allowed(&["tools", "tool_rounds", "unattended"]));
    }

    #[test]
    fn a_host_access_table_is_rejected_as_a_whole() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
[tools.host_access]
enabled = true

[tools.tool_rounds]
unattended = 100
"#,
        ));
        assert_eq!(rejected, vec!["tools.host_access".to_string()]);
        assert!(kept["tools"].get("host_access").is_none());
        assert_eq!(
            kept["tools"]["tool_rounds"]["unattended"].as_integer(),
            Some(100)
        );
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
    fn credentials_are_dropped_and_reported() {
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
        // `tools` had nothing but a credential in it, so the empty table is
        // pruned too.
        assert!(kept.get("tools").is_none());
    }

    #[test]
    fn a_provider_is_shared_without_its_key() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
[providers.local]
type = "openai_compatible"
base_url = "https://llm.example/v1"
model = "qwen"
api_key = "sk-secret"
"#,
        ));
        assert_eq!(rejected, vec!["providers.local.api_key".to_string()]);
        assert_eq!(
            kept["providers"]["local"]["base_url"].as_str(),
            Some("https://llm.example/v1")
        );
        assert!(kept["providers"]["local"].get("api_key").is_none());
    }

    #[test]
    fn a_whole_array_of_credentials_is_rejected_as_a_unit() {
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
    fn an_array_of_tables_is_searched_element_by_element() {
        let (kept, rejected) = filter_allowed(parse(
            r#"
[[tools.mcp_servers]]
name = "remote"
type = "http"
url = "https://mcp.example/mcp"
api_key = "sk-secret"

[[tools.mcp_servers]]
name = "local"
type = "stdio"
command = "sapphire-journal"
env = { GITHUB_TOKEN = "ghp-secret", RUST_LOG = "info" }
"#,
        ));
        assert_eq!(
            rejected,
            vec![
                "tools.mcp_servers.0.api_key".to_string(),
                "tools.mcp_servers.1.env.GITHUB_TOKEN".to_string()
            ]
        );
        let servers = kept["tools"]["mcp_servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["url"].as_str(), Some("https://mcp.example/mcp"));
        assert!(servers[0].get("api_key").is_none());
        assert_eq!(servers[1]["env"]["RUST_LOG"].as_str(), Some("info"));
        assert!(servers[1]["env"].get("GITHUB_TOKEN").is_none());
    }

    #[test]
    fn rejected_paths_are_sorted() {
        let (_, rejected) = filter_allowed(parse(
            r#"
workspace_dir = "/tmp/ws"

[discord]
bot_token = "b"

[matrix]
access_token = "t"
"#,
        ));
        assert_eq!(
            rejected,
            vec![
                "discord.bot_token".to_string(),
                "matrix.access_token".to_string(),
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

    /// A realistic workspace-level config: everything but the credentials,
    /// which the host layer adds.
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

# `cycles` belongs to a preset, not to `[timer]` itself, and `steps` is a
# required field — this block has to deserialize into `Config` for the
# round-trip test below.
[[timer.preset]]
name = "pomodoro"
cycles = 4

[[timer.preset.steps]]
label = "Focus"
minutes = 25.0

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
base_url = "http://llm.lan:8080/v1"
model = "qwen"
provider_name = "local"
max_tokens = 4096

[voice_pipeline.desk]
stt_provider = "whisper"
tts_provider = "piper"

[tools.tool_rounds]
unattended = 100

[[tools.mcp_servers]]
name = "sapphire-journal"
type = "stdio"
command = "sapphire-journal"
args = ["mcp"]
trust = "edit"
"#;

    #[test]
    fn the_fixture_has_no_credentials_to_drop() {
        let (_, rejected) = filter_allowed(parse(FIXTURE));
        assert!(
            rejected.is_empty(),
            "fixture has keys the workspace layer refuses: {rejected:?}"
        );
    }

    #[test]
    fn every_fixture_key_reaches_a_real_config_field() {
        // `Config` does not deny unknown fields, so a fixture key naming a
        // renamed or deleted field would otherwise be ignored in silence.
        // Round-tripping through the type catches it: serde drops an unknown
        // key on the way in, so it is missing on the way out. The host layer
        // holds only the keys — the shape a real deployment takes: the
        // workspace says where each provider lives and what to run there, the
        // host says how to authenticate.
        let host = parse(
            r#"
[anthropic]
api_key = "sk-test"

[providers.local]
api_key = "sk-local"
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
