//! Who may run which tool.
//!
//! One pure function, deliberately. There are two tool-calling loops in
//! this crate — `run_llm_turn` (`src/serve/mod.rs`) and the one inside
//! `Agent::handle_message` (`src/agent.rs`) — and the thing that must
//! not be duplicated between them is the *decision*. Merging the loops
//! themselves is a separate job, tracked in the design spec.
//!
//! Nothing here does I/O or knows about persistence: an `AllowAlways` is
//! recorded by the caller, not by `allow`.

use crate::config::{Config, ToolGroup};
use crate::tools::ToolKind;

/// The ACP session modes this agent offers.
///
/// Three, deliberately. `plan` is not "ask or don't ask" but "don't act
/// at all, produce a plan", which needs a different system prompt and a
/// way to present the plan — a separate feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Default,
    AcceptEdits,
    Bypass,
}

impl SessionMode {
    /// Every mode, in the order they are advertised to the client.
    pub const ALL: [SessionMode; 3] = [
        SessionMode::Default,
        SessionMode::AcceptEdits,
        SessionMode::Bypass,
    ];

    /// The wire id, as it appears in `session/set_mode`.
    pub fn id(self) -> &'static str {
        match self {
            SessionMode::Default => "default",
            SessionMode::AcceptEdits => "accept_edits",
            SessionMode::Bypass => "bypass",
        }
    }

    /// Human-readable name for the client's mode picker.
    pub fn name(self) -> &'static str {
        match self {
            SessionMode::Default => "Ask before editing",
            SessionMode::AcceptEdits => "Accept edits",
            SessionMode::Bypass => "Bypass permissions",
        }
    }

    /// One line under the name, saying what the mode actually changes.
    pub fn description(self) -> &'static str {
        match self {
            SessionMode::Default => "Ask before writing files or running commands.",
            SessionMode::AcceptEdits => {
                "Write files without asking; still ask before running commands."
            }
            SessionMode::Bypass => "Run everything without asking.",
        }
    }

    /// `None` for an id this agent does not implement — `plan`, notably.
    pub fn from_id(id: &str) -> Option<SessionMode> {
        SessionMode::ALL.into_iter().find(|m| m.id() == id)
    }
}

/// Origin and session-profile name, captured for the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Acp {
        mode: SessionMode,
        session_profile: String,
    },
    Channel {
        session_profile: String,
    },
    Unattended {
        session_profile: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // Public policy vocabulary: Allow::Allow.
pub enum Allow {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone)]
pub struct ResolvedPermission {
    pub name: String,
    pub permission: String,
    pub groups: Vec<ToolGroup>,
    pub mode: Option<SessionMode>,
}

impl ResolvedPermission {
    pub fn resolve(config: &Config, origin: Origin) -> Self {
        let (name, mode) = match origin {
            Origin::Acp {
                mode,
                session_profile,
            } => (session_profile, Some(mode)),
            Origin::Channel { session_profile } | Origin::Unattended { session_profile } => {
                (session_profile, None)
            }
        };
        let permission = config.permission_profile_name_for_session(&name);
        Self {
            groups: config.permission_profile(&permission).tools,
            name,
            permission,
            mode,
        }
    }

    pub fn lists(&self, group: ToolGroup) -> bool {
        self.groups.contains(&group)
    }
}

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub name: String,
    pub group: Option<ToolGroup>,
    pub enabled: bool,
    pub kind: ToolKind,
}

impl ToolEntry {
    pub(crate) fn from_spec(spec: &crate::provider::ToolSpec, config: &Config) -> Self {
        Self {
            name: spec.name.to_string(),
            group: spec.group,
            kind: spec.kind,
            enabled: spec.group.is_some_and(|g| config.group_enabled(g)),
        }
    }
}

/// Group access is the intersection of the session grant and host capabilities;
/// the ACP mode then decides whether the tool's kind requires approval.
pub fn allow(profile: &ResolvedPermission, entry: &ToolEntry) -> Allow {
    if entry
        .group
        .is_some_and(|g| !entry.enabled || !profile.lists(g))
    {
        return Allow::Deny;
    }
    let risky = matches!(entry.kind, ToolKind::Execute | ToolKind::Other);
    let safe = matches!(
        entry.kind,
        ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Think
    );
    match profile.mode {
        None | Some(SessionMode::Bypass) => Allow::Allow,
        Some(_) if safe => Allow::Allow,
        Some(SessionMode::AcceptEdits) if risky => Allow::Ask,
        Some(SessionMode::AcceptEdits) => Allow::Allow,
        Some(SessionMode::Default) => Allow::Ask,
    }
}

/// The outcome of asking. Maps 1:1 onto ACP's `PermissionOptionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl Approval {
    /// Whether the call runs.
    pub fn allows(self) -> bool {
        matches!(self, Approval::AllowOnce | Approval::AllowAlways)
    }

    /// Whether this answer should outlive the call that prompted it.
    pub fn is_sticky(self) -> bool {
        matches!(self, Approval::AllowAlways | Approval::RejectAlways)
    }
}

/// Why a call was refused. The model reads the difference: one is
/// worth rephrasing around, the other is not worth retrying at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A human was asked and said no.
    UserDeclined,
    /// The policy refuses it outright on this transport; nobody was asked.
    Unavailable {
        session: String,
        permission: String,
        group: ToolGroup,
    },
    /// The name does not appear in this round's own `tool_specs` — it
    /// was never offered to this conversation, whether or not it is
    /// registered on the shared `ToolSet`. Distinct from `Unavailable`:
    /// that reason is about the deployment (host access, a missing
    /// editor); this one is about what *this* call is allowed to see —
    /// a subagent's `tools:` restriction, or `subagent` itself once
    /// removed from a nested turn's own list.
    NotOffered,
    /// The call needed a human's answer and the turn has nobody to ask:
    /// a background task. Distinct from `UserDeclined`, because nobody
    /// said no — the model should report the call, not treat it as a
    /// decision the user made.
    NeedsApproval,
}

/// What the model is told in place of a result.
///
/// One function rather than a literal at each call site: both tool
/// loops refuse calls, and two copies of this wording would drift.
pub fn refusal_message(tool: &str, why: Refusal) -> String {
    match why {
        Refusal::UserDeclined => format!(
            "Permission denied: the user declined the '{tool}' call. \
             Do not retry it without being asked to."
        ),
        Refusal::Unavailable {
            session,
            permission,
            group,
        } => format!(
            "Permission denied: the '{tool}' tool is unavailable for session profile '{session}'; check permission_profile.{permission}.tools and [tools.{}].enabled.",
            format!("{group:?}").to_lowercase()
        ),
        // Deliberately not "Permission denied": for a name this round
        // never offered — a typo, a hallucination, or `subagent`
        // reaching past its own depth cap — there is no permission to
        // ask for. Opening with "Permission denied" would invite the
        // model to ask the user to grant access to something that does
        // not exist here at all.
        Refusal::NotOffered => format!(
            "The '{tool}' tool is not available in this conversation. \
             Try another approach, or ask the user."
        ),
        Refusal::NeedsApproval => format!(
            "{tool} needs the user's approval, which a background task cannot \
             ask for. Report it in your answer instead of retrying."
        ),
    }
}

/// Explain the group policy refusal using the actual grant and host switch.
pub fn unavailable_message(profile: &ResolvedPermission, entry: &ToolEntry) -> String {
    let group = entry.group.expect("only grouped tools can be denied");
    refusal_message(
        &entry.name,
        Refusal::Unavailable {
            session: profile.name.clone(),
            permission: profile.permission.clone(),
            group,
        },
    )
}

/// Split non-interactive calls, failing closed for unknown names or Ask.
pub fn partition_without_asking(
    profile: &ResolvedPermission,
    calls: &[crate::provider::ToolCall],
    entries: &[ToolEntry],
) -> (Vec<crate::provider::ToolCall>, Vec<(String, String)>) {
    let mut permitted = Vec::with_capacity(calls.len());
    let mut refused = Vec::new();
    for call in calls {
        match entries.iter().find(|entry| entry.name == call.name) {
            Some(entry) if allow(profile, entry) == Allow::Allow => permitted.push(call.clone()),
            Some(entry) if allow(profile, entry) == Allow::Deny => {
                refused.push((call.id.clone(), unavailable_message(profile, entry)))
            }
            Some(_) => refused.push((
                call.id.clone(),
                refusal_message(&call.name, Refusal::NeedsApproval),
            )),
            None => refused.push((
                call.id.clone(),
                refusal_message(&call.name, Refusal::NotOffered),
            )),
        }
    }
    (permitted, refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ToolGroup};

    /// R2/R9: the table is group × permission profile, and the mode is what decides
    /// whether an *offered* one asks, from `kind` — today's `decide` rows, unchanged,
    /// including for a tool that carries no group.
    #[tokio::test]
    async fn allow_is_the_group_table_and_the_mode_row() {
        let entry = |name: &str, group: Option<ToolGroup>, enabled: bool, kind| ToolEntry {
            name: name.into(),
            group,
            enabled,
            kind,
        };
        let default_fs = ResolvedPermission {
            name: "default".into(),
            permission: "default".into(),
            groups: vec![ToolGroup::Fs],
            mode: None,
        };
        // A channel turn under `default`: `fs` is listed and enabled here, so it runs unasked.
        assert_eq!(
            allow(
                &default_fs,
                &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)
            ),
            Allow::Allow
        );
        // `shell` is refused because no profile named it (decision 3), whatever its kind.
        assert_eq!(
            allow(
                &default_fs,
                &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)
            ),
            Allow::Deny
        );
        // R2's ∧ half: a group this host does not hand the agent is refused whatever lists it.
        assert_eq!(
            allow(
                &default_fs,
                &entry("file_write", Some(ToolGroup::Fs), false, ToolKind::Edit)
            ),
            Allow::Deny
        );

        // R9: under `default` an offered edit asks; under `accept_edits` it does not, and
        // only `Execute`/`Other` still do; `bypass` asks nothing.
        let edits = ResolvedPermission {
            name: "default".into(),
            permission: "default".into(),
            groups: vec![ToolGroup::Fs],
            mode: Some(SessionMode::Default),
        };
        assert_eq!(
            allow(
                &edits,
                &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)
            ),
            Allow::Ask
        );
        let accepted = ResolvedPermission {
            groups: vec![ToolGroup::Fs, ToolGroup::Shell],
            mode: Some(SessionMode::AcceptEdits),
            ..edits.clone()
        };
        assert_eq!(
            allow(
                &accepted,
                &entry("file_write", Some(ToolGroup::Fs), true, ToolKind::Edit)
            ),
            Allow::Allow
        );
        assert_eq!(
            allow(
                &accepted,
                &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)
            ),
            Allow::Ask
        );
        let bypass = ResolvedPermission {
            groups: vec![ToolGroup::Fs, ToolGroup::Shell],
            mode: Some(SessionMode::Bypass),
            ..edits.clone()
        };
        assert_eq!(
            allow(
                &bypass,
                &entry("shell", Some(ToolGroup::Shell), true, ToolKind::Execute)
            ),
            Allow::Allow
        );
    }

    /// R4: the always-allowed tools are outside the table's subject — `enabled` and the
    /// profile's listing are both beside the point. Their `kind` still meets the mode
    /// exactly as a grouped tool's does: the old early-return that would have skipped it dies.
    #[tokio::test]
    async fn always_allowed_tools_are_outside_the_table() {
        let chat = ResolvedPermission {
            name: "chat".into(),
            permission: "chat".into(),
            groups: vec![],
            mode: None,
        };
        for kind in [
            ToolKind::Read,
            ToolKind::Fetch,
            ToolKind::Search,
            ToolKind::Think,
        ] {
            let entry = ToolEntry {
                name: "current_time".into(),
                group: None,
                enabled: false,
                kind,
            };
            assert_eq!(allow(&chat, &entry), Allow::Allow, "{kind:?}");
        }
        // A delegation tool with no group is still asked under a mode that asks.
        let acp_default = ResolvedPermission {
            name: "chat".into(),
            permission: "chat".into(),
            groups: vec![],
            mode: Some(SessionMode::Default),
        };
        assert_eq!(
            allow(
                &acp_default,
                &ToolEntry {
                    name: "subagent".into(),
                    group: None,
                    enabled: false,
                    kind: ToolKind::Other
                }
            ),
            Allow::Ask
        );
        assert_eq!(
            allow(
                &acp_default,
                &ToolEntry {
                    name: "memory_read".into(),
                    group: None,
                    enabled: false,
                    kind: ToolKind::Read
                }
            ),
            Allow::Allow
        );
    }

    /// A channel turn never asks: the partition sees Allow and Deny only, and every refusal
    /// names the setting that would change it.
    #[tokio::test]
    async fn a_channel_turn_partitions_on_allow_alone() {
        let cfg = Config::parse_for_test(
            "[anthropic]\napi_key = \"test\"\n\n\
             [session_profile.ops]\nprofile = \"dev\"\npermission = \"default\"\n\n\
             [tools.fs]\nenabled = true\n[tools.shell]\nenabled = true\n",
        );
        // Declared wins verbatim; its `permission` names `default`, and `default` lists fs
        // and mcp — never shell.
        let profile = ResolvedPermission::resolve(
            &cfg,
            Origin::Channel {
                session_profile: cfg.session_profile_name(Some("ops"), "!a:srv"),
            },
        );
        assert_eq!(
            profile.name, "ops",
            "the turn carries the session-profile name it came with"
        );
        assert_eq!(
            profile.permission, "default",
            "and the permission profile is what the chain resolves to"
        );

        let entries = [
            ToolEntry {
                name: "file_write".into(),
                group: Some(ToolGroup::Fs),
                enabled: cfg.group_enabled(ToolGroup::Fs),
                kind: ToolKind::Edit,
            },
            ToolEntry {
                name: "shell".into(),
                group: Some(ToolGroup::Shell),
                enabled: cfg.group_enabled(ToolGroup::Shell),
                kind: ToolKind::Execute,
            },
            ToolEntry {
                name: "memory_add".into(),
                group: None,
                enabled: false,
                kind: ToolKind::Edit,
            },
        ];
        let calls: Vec<crate::provider::ToolCall> =
            [("file_write", "c0"), ("shell", "c1"), ("memory_add", "c2")]
                .into_iter()
                .map(|(name, id)| crate::provider::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    input: serde_json::json!({}),
                })
                .collect();

        let (permitted, refused) = partition_without_asking(&profile, &calls, &entries);
        let kept: Vec<&str> = permitted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kept, vec!["file_write", "memory_add"]);
        let name_of = |id: &str| {
            calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
                .unwrap()
        };
        let blocked: Vec<String> = refused.iter().map(|(id, _)| name_of(id)).collect();
        assert_eq!(blocked, vec!["shell"]);
        for (id, reason) in &refused {
            assert!(reason.contains(&name_of(id)), "{reason} names its tool");
            assert!(
                reason.contains("permission_profile"),
                "{reason} names the setting to change"
            );
        }
        assert_eq!(permitted.len() + refused.len(), calls.len());
    }
}
