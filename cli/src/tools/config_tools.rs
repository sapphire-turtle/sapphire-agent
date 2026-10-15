//! The agent's own definition files, as tools (#265).
//!
//! `<workspace>/heartbeat/*.md`, `<workspace>/autonomous/*.md` and
//! `<workspace>/agents/*.md` are the files an operator hand-edits to
//! describe work the agent does on its own. One `ConfigTool`, registered
//! three times with only the directory differing, is what lets a chat
//! room read and write them.
//!
//! Two properties hold the design together:
//!
//! - **A room profile must be named.** `[tools.admin].room_profiles` is a
//!   host-layer list of room-profile names, and every action refuses in a
//!   session whose profile is not on it. A channel room resolves to its
//!   profile the same way its provider does; an ACP session carries the
//!   profile its bearer token pinned at `session/new` (or `load`/`resume`),
//!   which is what lets an editor manage definitions at all — ACP has no
//!   room id. The transports that name no profile (`/rpc`, A2A, voice, the
//!   unattended loops) get `None`, which is a refusal.
//! - **The write side refuses what the loader would drop.** `parse_definition`
//!   is the loader's own parse, failing instead of skipping, so a
//!   definition that would never fire cannot be written and then puzzled
//!   over.

use crate::autonomous::{CONTINUE_PROMPT, marker};
use crate::config::{Config, DEFAULT_NAMESPACE_NAME};
use crate::provider::ChatMessage;
use crate::provider::ToolSpec;
use crate::serve::{AutonomousHost, LlmTurnOutcome, ServeState, TurnStop};
use crate::tools::subagent::SubagentTool;
use crate::tools::{Tool, ToolKind, ToolSet};
use crate::workspace::slash_path;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sapphire_framework::workspace::WorkspaceState;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tracing::warn;

/// Which of the three definition directories a [`ConfigTool`] speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDir {
    Heartbeat,
    Autonomous,
    Agents,
}

impl ConfigDir {
    /// Every directory, in the order the tool names sort. Task 7 walks
    /// this to register one tool per entry.
    pub const ALL: [ConfigDir; 3] = [
        ConfigDir::Agents,
        ConfigDir::Autonomous,
        ConfigDir::Heartbeat,
    ];

    /// The directory under the workspace root that holds the definitions.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ConfigDir::Heartbeat => "heartbeat",
            ConfigDir::Autonomous => "autonomous",
            ConfigDir::Agents => "agents",
        }
    }

    /// The tool name this directory is registered under.
    pub fn tool_name(&self) -> &'static str {
        match self {
            ConfigDir::Heartbeat => "heartbeat_config",
            ConfigDir::Autonomous => "autonomous_config",
            ConfigDir::Agents => "agent_config",
        }
    }

    /// Whether a definition here has an `enabled:` flag to flip.
    ///
    /// A subagent definition does not: it runs when it is called, which
    /// is a decision the parent model makes per delegation rather than an
    /// on/off switch on the file. Offering `set_enabled` there would mean
    /// inventing a flag nothing reads.
    pub fn supports_enabled(&self) -> bool {
        !matches!(self, Self::Agents)
    }
}

/// Resolve a definition name to `<workspace_root>/<dir>/<stem>.md`.
///
/// The name has to be a bare file stem. `file_write` accepts absolute
/// paths and `~`, but this does not, deliberately: the value of a tool
/// that writes the agent's own definitions is that its intent is legible
/// from the call, and `name` being one word of a file in one directory is
/// what makes it so. Anything that could name a *different* file — a
/// separator, a leading dot, a `..` — is refused rather than normalised.
pub(crate) fn definition_path(workspace_root: &Path, dir: &str, name: &str) -> Result<PathBuf> {
    // `.md` in any case names the same file on a case-insensitive
    // filesystem, so `Foo.MD` must not become `Foo.MD.md`.
    let bytes = name.as_bytes();
    let stem = if name.len() >= 3 && bytes[name.len() - 3..].eq_ignore_ascii_case(b".md") {
        &name[..name.len() - 3]
    } else {
        name
    };
    if stem.is_empty()
        || stem.starts_with('.')
        || stem.contains('/')
        || stem.contains('\\')
        || stem.contains("..")
    {
        anyhow::bail!(
            "{name:?} is not a definition name: pass the bare file stem, e.g. \
             \"morning_call\", with no directory and no path"
        );
    }
    Ok(workspace_root.join(dir).join(format!("{stem}.md")))
}

tokio::task_local! {
    /// The room profile an ACP turn runs under, scoped around tool execution
    /// by `TurnLoop::run` from the profile pinned at `session/new` (or
    /// `load`/`resume`). `None` on every other serve-path turn.
    static ADMIN_ROOM_PROFILE_TL: Option<String>;
}

/// Run `fut` with the turn's room profile reachable from
/// [`current_admin_room_profile`]. Called once per round by
/// `TurnLoop::run`, wrapping the same tool execution the timer and ACP-client
/// scopes wrap.
///
pub(crate) fn scope_admin_room_profile<F: std::future::Future>(
    profile: Option<String>,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    ADMIN_ROOM_PROFILE_TL.scope(profile, fut)
}

/// The room profile scoped around the tool call currently executing, if the
/// turn came in on a transport that pins one — today only ACP.
///
/// `None` outside `scope_admin_room_profile` and `None` for a `/rpc`, A2A or
/// unattended turn, which pins no profile. The two are the same answer on
/// purpose: neither is a place an operator declared.
pub(crate) fn current_admin_room_profile() -> Option<String> {
    ADMIN_ROOM_PROFILE_TL.try_with(Clone::clone).ok().flatten()
}

/// The room profile this call's admin grant is judged by, or `None` when the
/// transport names no profile an operator could have allowed.
///
/// A channel turn (`Agent::handle_message`, and the heartbeat's chat leg
/// through it) carries `TimerOrigin::Chat`, so its room resolves to a profile
/// exactly as its provider does — the grant follows the room, which is why a
/// heartbeat fired *into* a listed room can call these tools with nobody
/// present. Voice is a device, not a room or an editor. With no timer origin
/// the turn is on the serve path: an ACP turn scopes the profile its bearer
/// token pinned, while `/rpc`, A2A and the unattended loops scope nothing.
pub(crate) fn current_admin_room_profile_for(config: &Config) -> Option<String> {
    match crate::timer::current_origin() {
        Some(crate::timer::TimerOrigin::Chat { room_id }) => {
            Some(config.room_profile_name_for_room(&room_id).to_string())
        }
        Some(crate::timer::TimerOrigin::Voice { .. }) => None,
        None => current_admin_room_profile(),
    }
}

/// What every action answers with when the calling room profile is not on
/// `[tools.admin].room_profiles`.
///
/// The wording names the config key on purpose: a model that is refused has
/// no other way to find out, and an operator being told by the agent *which*
/// setting to change is the whole reason the refusal is not just "denied".
pub(crate) const ROOM_REFUSAL: &str = "Permission denied: the config tools are not available for this room profile. An operator can allow them with `[tools.admin].room_profiles` in the host config.";

/// The *effective* `enabled` value of a definition, as the loaders read
/// it: `enabled:` when present, `true` when absent (both loaders declare
/// `#[serde(default = "default_true")]`, so an unmentioned definition is
/// an enabled one), and `None` when there is no frontmatter at all or the
/// `enabled:` value itself is unparseable (the loader skips such a file,
/// so there is no effective state to claim).
///
/// Only a top-level `enabled:` counts, matching `frontmatter::set_enabled`
/// — an indented one belongs to a nested mapping such as `voice:`.
pub(crate) fn declared_enabled(raw: &str) -> Option<bool> {
    let (fm, _) = crate::frontmatter::split(raw)?;
    for line in fm.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if let Some(value) = trimmed.strip_prefix("enabled:") {
            let value = value.trim();
            // A value serde would reject leaves the file *skipped* by the
            // loader, so there is no effective state to claim: `None`,
            // the same answer as a missing frontmatter block. The
            // loaders' `default_true` applies only when the key is
            // ABSENT, which the `Some(true)` at the end covers.
            return value.parse::<bool>().ok();
        }
    }
    Some(true)
}

fn lock(state: &WorkspaceState) -> &WorkspaceState {
    state
}

/// `list` / `read` / `write` / `set_enabled` over one definition
/// directory, gated on the calling room and on content the loaders can
/// actually use.
pub struct ConfigTool {
    dir: ConfigDir,
    workspace_root: PathBuf,
    config: Config,
    /// The workspace's own writer, not `std::fs`, so a definition written
    /// here lands in `workspace_search`'s index like any other file —
    /// same path `memory_add` takes.
    ws: Arc<WorkspaceState>,
    /// The live `subagent` tool, for `Agents` only: a definition written
    /// while the agent runs is swapped into it so the next delegation can
    /// use it. `Weak` because `ToolSet` owns the tool and the tool must
    /// not own the `ToolSet` back (see `after_write`).
    subagent: Option<Weak<SubagentTool>>,
    /// The `ToolSet` this tool is registered in, for `Agents` only: the
    /// spec the model is *offered* for `subagent` lives there, so
    /// refreshing the tool without refreshing that spec would leave a
    /// definition callable but unadvertised.
    tool_set: Weak<ToolSet>,
    spec: ToolSpec,
}

impl ConfigTool {
    pub fn new(
        dir: ConfigDir,
        workspace_root: PathBuf,
        config: Config,
        ws: Arc<WorkspaceState>,
        subagent: Option<Weak<SubagentTool>>,
        tool_set: Weak<ToolSet>,
    ) -> Self {
        let actions: Vec<&str> = if dir.supports_enabled() {
            vec!["list", "read", "write", "set_enabled"]
        } else {
            vec!["list", "read", "write"]
        };

        let mut description = format!(
            "Read and write the agent's own {name} definitions: the Markdown files with \
             YAML frontmatter under `<workspace>/{dir}/*.md`. `list` reports each \
             definition and, for each, whether it is effectively enabled; `read` returns \
             one exactly as it is on disk; `write` creates or replaces one, refusing \
             content the agent's own loader would drop. `set_enabled` changes only the \
             `enabled:` line of an existing definition — comments, other keys and the \
             body are left byte-for-byte as they were — so prefer it over `write` when \
             all you want is to turn something on or off.",
            name = dir.tool_name(),
            dir = dir.dir_name(),
        );
        if dir == ConfigDir::Agents {
            description.push_str(
                " A subagent definition has no `enabled:` flag — it runs when it is \
                 called — so `set_enabled` is not one of this tool's actions. A \
                 definition written here is offered for delegation immediately, without \
                 restarting the agent.",
            );
        }
        description.push_str(
            " These tools are available only in sessions whose room profile an operator \
             listed in `[tools.admin].room_profiles`; anywhere else, every action including \
             `list` is refused.",
        );

        let spec = ToolSpec {
            name: dir.tool_name().into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": actions,
                        "description": "Which operation to perform.",
                    },
                    "name": {
                        "type": "string",
                        "description": "The definition's file stem — one word, no \
                            directory and no extension (e.g. \"morning_call\").",
                    },
                    "content": {
                        "type": "string",
                        "description": "For `write`: the whole file, frontmatter and \
                            body, which replaces any existing definition of that name.",
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "For `set_enabled`: whether the definition \
                            should be enabled.",
                    },
                },
                "required": ["action"],
            }),
        };

        Self {
            dir,
            workspace_root,
            config,
            ws,
            subagent,
            tool_set,
            spec,
        }
    }

    /// The directory this tool reads and writes.
    fn dir_path(&self) -> PathBuf {
        self.workspace_root.join(self.dir.dir_name())
    }

    /// A workspace-relative path for `WorkspaceState::write_file`.
    ///
    /// The path is built from a fixed directory name and a validated stem,
    /// so it is inside the workspace — *assuming no symlink is pre-placed
    /// in the definition directory*. `main` builds the `AppContext` with
    /// `.allow_external_paths()`, so a pre-placed symlink resolving out of
    /// the tree is followed rather than refused: the write lands at the
    /// symlink's target. That is accepted defense-in-depth, not the
    /// invariant this check provides — what it catches is a path that is
    /// outside the workspace *before* any filesystem resolution, which is
    /// the only case the stem validation leaves open.
    fn rel(&self, abs: &Path) -> Result<PathBuf> {
        abs.strip_prefix(&self.workspace_root)
            .map(Path::to_path_buf)
            .map_err(|_| anyhow!("refusing to touch {}: outside the workspace", abs.display()))
    }

    /// The profile gate. A refusal here is the only permission check these
    /// tools have — they are `ToolKind::Edit` and deliberately outside
    /// `ToolPolicy`, because the grant that matters is the host's room-profile
    /// list, not a per-tool policy the workspace can set.
    fn gate(&self) -> Result<()> {
        if current_admin_room_profile_for(&self.config)
            .is_some_and(|name| self.config.admin_allows_room_profile(&name))
        {
            Ok(())
        } else {
            Err(anyhow!(ROOM_REFUSAL))
        }
    }

    /// What this tool's actions are, for an error message. Names the
    /// missing `set_enabled` where there is not one, since a model that
    /// asked for it is exactly the reader who needs to be told why.
    fn actions_help(&self) -> String {
        if self.dir.supports_enabled() {
            format!(
                "{} accepts `list`, `read`, `write`, `set_enabled`.",
                self.dir.tool_name()
            )
        } else {
            format!(
                "{} accepts `list`, `read`, `write`. A subagent definition runs when it \
                 is called, so it has no on/off state for `set_enabled` to flip — \
                 rewrite it with `write`.",
                self.dir.tool_name()
            )
        }
    }

    fn list(&self) -> Result<String> {
        let dir = self.dir_path();
        let mut lines: Vec<String> = Vec::new();
        // Plain `std::fs`, the loader's own path, so the listing matches
        // what the agent would actually load rather than what the
        // workspace index happens to know.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !self.dir.supports_enabled() {
                    lines.push(stem.to_string());
                    continue;
                }
                match std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| declared_enabled(&raw))
                {
                    Some(true) => lines.push(format!("{stem} (enabled)")),
                    Some(false) => lines.push(format!("{stem} (disabled)")),
                    // No frontmatter: not a definition the loader loads
                    // at all, so it has no effective state to report.
                    // Still listed, since hiding a file the operator can
                    // see would be the more confusing of the two.
                    None => lines.push(stem.to_string()),
                }
            }
        }
        if lines.is_empty() {
            return Ok(format!("No {} definitions.", self.dir.dir_name()));
        }
        lines.sort();
        Ok(lines.join("\n"))
    }

    fn read(&self, input: &serde_json::Value) -> Result<String> {
        let name = input["name"].as_str().context("missing 'name'")?;
        let abs = definition_path(&self.workspace_root, self.dir.dir_name(), name)?;
        // Verbatim, not through `serde`: a definition is a file people
        // hand-edit, and the model has to see the comments that say why.
        // Propagated, not swallowed: a path that is not in the workspace
        // is exactly the case worth reporting, and the `?` names it
        // rather than falling back to the absolute path.
        let rel = self.rel(&abs)?;
        std::fs::read_to_string(&abs)
            .with_context(|| format!("failed to read {}", slash_path(&rel)))
    }

    async fn write(&self, input: &serde_json::Value) -> Result<String> {
        let name = input["name"].as_str().context("missing 'name'")?;
        let content = input["content"].as_str().context("missing 'content'")?;
        let abs = definition_path(&self.workspace_root, self.dir.dir_name(), name)?;
        // Refuse before anything touches the disk: a definition the
        // loader would drop must not be left behind for the model to
        // find on the next `list` and believe.
        self.validate(name, content)?;
        let rel = self.rel(&abs)?;
        {
            let state = lock(&self.ws);
            state
                .write_file(&rel, content)
                .with_context(|| format!("failed to write {}", slash_path(&rel)))?;
        }
        self.after_write().await;
        Ok(format!("Wrote {}", slash_path(&rel)))
    }

    async fn set_enabled(&self, input: &serde_json::Value) -> Result<String> {
        if !self.dir.supports_enabled() {
            anyhow::bail!(
                "{} does not support the action `set_enabled`. {}",
                self.dir.tool_name(),
                self.actions_help()
            );
        }
        let name = input["name"].as_str().context("missing 'name'")?;
        let enabled = input["enabled"]
            .as_bool()
            .context("missing 'enabled' (true or false)")?;
        let abs = definition_path(&self.workspace_root, self.dir.dir_name(), name)?;
        let rel = self.rel(&abs)?;
        let raw = std::fs::read_to_string(&abs)
            .with_context(|| format!("failed to read {}", slash_path(&rel)))?;
        let Some(updated) = crate::frontmatter::set_enabled(&raw, enabled) else {
            // Not repaired by inventing a frontmatter block: the file is
            // markdown the agent does not load, and pretending otherwise
            // would be this tool writing a definition nobody asked for.
            anyhow::bail!(
                "{} has no YAML frontmatter, so there is no `enabled:` key to set",
                slash_path(&rel)
            );
        };
        {
            let state = lock(&self.ws);
            state
                .write_file(&rel, &updated)
                .with_context(|| format!("failed to write {}", slash_path(&rel)))?;
        }
        Ok(format!("Set enabled: {enabled} in {}", slash_path(&rel)))
    }

    /// Refuse content the corresponding loader would not be able to use.
    ///
    /// Each arm is the loader's own parse (`parse_definition`), plus the
    /// checks that loader applies elsewhere — a heartbeat `schedule:` the
    /// cron parser cannot read, and the subagent profile references
    /// `main` validates at startup.
    fn validate(&self, name: &str, content: &str) -> Result<()> {
        match self.dir {
            ConfigDir::Heartbeat => {
                let task = crate::heartbeat_config::parse_definition(name, content)
                    .map_err(|e| anyhow!("refusing to write {name}.md: {e}"))?;
                // A task whose cron does not parse is *kept* by the
                // loader and skipped by `next_due`, which is a different
                // failure from a file that is skipped — and the one that
                // matters on the write side, since the file would sit
                // there looking enabled and never fire.
                if task.parsed_schedule().is_none() {
                    anyhow::bail!(
                        "refusing to write {name}.md: the `schedule:` value is not a cron \
                         expression the agent can read, so the task would never fire"
                    );
                }
                Ok(())
            }
            ConfigDir::Autonomous => {
                crate::autonomous_config::parse_definition(name, content)
                    .map_err(|e| anyhow!("refusing to write {name}.md: {e}"))?;
                Ok(())
            }
            ConfigDir::Agents => {
                let def = crate::agents::parse_definition(name, content)
                    .map_err(|e| anyhow!("refusing to write {name}.md: {e}"))?;
                // The same check `main` runs at startup. Now that a
                // definition can be written while the agent is running,
                // leaving it out here would be a way around the startup
                // check — and the failure would be an agent quietly
                // running on a provider nobody chose.
                let errors = self
                    .config
                    .validate_subagent_profiles(std::slice::from_ref(&def));
                if !errors.is_empty() {
                    anyhow::bail!("refusing to write {name}.md: {}", errors.join("; "));
                }
                // A misspelled name in `tools:` is *not* refused: a
                // subagent warns about it and drops that one name, which
                // is a mistake in a line rather than a broken definition.
                Ok(())
            }
        }
    }

    /// Make a freshly written agent definition usable without a restart.
    ///
    /// Only `Agents` has state to refresh. Both back-references are
    /// `Weak` and either may be gone (the tool built for a test, or a
    /// deployment that did not wire them up); when that happens the file
    /// is still written and the change simply takes effect at the next
    /// start.
    ///
    /// The two halves go together: `set_agents` is what `execute`
    /// dispatches from, and `replace_spec` is what the model is offered.
    /// Refreshing only the first would make a definition callable but
    /// invisible.
    async fn after_write(&self) {
        if self.dir != ConfigDir::Agents {
            return;
        }
        let Some(subagent) = self.subagent.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        let Some(tool_set) = self.tool_set.upgrade() else {
            return;
        };
        subagent.set_agents(crate::agents::load_agents_dir(&self.dir_path()));
        tool_set
            .replace_spec(
                crate::tools::subagent::SUBAGENT_TOOL_NAME,
                subagent.live_spec(),
            )
            .await;
    }
}

#[async_trait]
impl Tool for ConfigTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // Before the action is even looked at: `list` is refused as
        // firmly as `write`, since knowing which definitions exist is
        // itself something the operator granted to a room.
        self.gate()?;
        match input["action"].as_str() {
            Some("list") => self.list(),
            Some("read") => self.read(input),
            Some("write") => self.write(input).await,
            Some("set_enabled") => self.set_enabled(input).await,
            other => anyhow::bail!(
                "{}: unknown action {:?}. {}",
                self.dir.tool_name(),
                other.unwrap_or(""),
                self.actions_help()
            ),
        }
    }
}

/// The most turns a `task_test` run may spend.
///
/// A tool whose job is to check that a task does not eat tokens must not
/// eat tokens itself, so the cap is a constant rather than a knob: a
/// definition's own `max_turns` can only ever lower it (see
/// [`capped_test_turns`]).
const MAX_TEST_TURNS: usize = 3;

/// The `room_id` a test session is created under.
///
/// **Deliberately not the task's name.** `autonomous::is_due` anchors a
/// task's cooldown on the latest session whose `room_id == task.name`, so
/// a test that claimed that name would reset the real task's cooldown —
/// merely testing a task would postpone it. The `test:` prefix keeps every
/// test session out of that query.
fn test_room_id(kind: &str, name: &str) -> String {
    format!("test:{kind}:{name}")
}

/// How many turns a requested `max_turns` means for a test run.
///
/// `Some(0)` is one turn — a zero-turn run would test nothing — and every
/// other request is capped at [`MAX_TEST_TURNS`]. `None` means "use the
/// definition's own `max_turns`"; that is signalled as `0` rather than an
/// `Option` so the caller has one number to resolve.
fn capped_test_turns(requested: Option<usize>) -> usize {
    match requested {
        Some(0) => 1,
        Some(n) => n.min(MAX_TEST_TURNS),
        None => 0,
    }
}

/// One line for why a test run ended.
///
/// `TurnStop`'s `Debug` prints `BudgetExhausted { partial_text: "..." }`,
/// which is not a sentence an operator can read in a report. Returns an
/// owned `String` rather than `&'static str`, unlike before: a provider
/// error now carries its own message, and an operator reading a
/// `task_test` report deserves the same cause a subagent's parent model
/// gets (see `TurnStop::ProviderError`'s doc), not just "provider error".
fn describe_stop(stop: &TurnStop) -> String {
    match stop {
        TurnStop::Replied => "replied".to_string(),
        TurnStop::ProviderError { message } => format!("provider error: {message}"),
        TurnStop::BudgetExhausted { .. } => "tool-round budget exhausted".to_string(),
    }
}

/// Run one of the agent's own task definitions once, before it is enabled.
///
/// The run is production's in every respect but two: the session it lands
/// in is named `test:<kind>:<name>` rather than after the task, and it
/// stops after [`MAX_TEST_TURNS`]. Same `serve::run_llm_turn`, same prompt
/// assembly, same permission row (`[autonomous] origin`) — a test that ran
/// with looser permissions than production would prove nothing.
///
/// `enabled: false` is not an obstacle: it is the case this exists for.
pub struct TaskTestTool {
    /// A strong `Arc<ServeState>`, so this tool and the state it runs on
    /// form a process-lifetime cycle (`ServeState` -> `ToolSet` ->
    /// `TaskTestTool` -> `ServeState`). Intentional and terminal: both
    /// live for the whole process, exactly as the timer wiring's
    /// `Weak<ServeState>` precedent exists to avoid *for a task that can
    /// outlive the state*. Nothing here does, so a `Weak` would only add
    /// an upgrade-or-refuse path with no reachable case.
    state: Arc<ServeState>,
    workspace_root: PathBuf,
    spec: ToolSpec,
}

impl TaskTestTool {
    pub fn new(state: Arc<ServeState>) -> Self {
        // Derived rather than passed: the workspace root is already in the
        // state, and a second copy could disagree with it.
        let workspace_root = state.workspace.dir().to_path_buf();
        let spec = ToolSpec {
            name: "task_test".into(),
            description: "Run one of the agent's own task definitions once, in a throwaway \
                 session, to see what it does before enabling it. The task runs even while \
                 `enabled: false` — that is the point: try it first, then turn it on with \
                 `heartbeat_config` / `autonomous_config` and `set_enabled`. The run uses the \
                 same model, the same prompt assembly and the same permission row as the real \
                 thing, but its session is named `test:<kind>:<name>` instead of after the \
                 task, so it touches neither the `enabled:` flag nor the task's cooldown. \
                 `max_turns` caps an autonomous test at 3 turns; a heartbeat task is one \
                 prompt and is always exactly one turn. Delivery is not verified: a heartbeat \
                 task's `room_id:` / `voice:` targets are ignored, because what this tool \
                 checks is the prompt and how the task ends, not where a result would go. \
                 These tools are available only in sessions whose room profile an operator \
                 listed in `[tools.admin].room_profiles`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["heartbeat", "autonomous"],
                        "description": "Which definition directory the task lives in.",
                    },
                    "name": {
                        "type": "string",
                        "description": "The definition's file stem — one word, no \
                            directory and no extension (e.g. \"journal\").",
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "For `autonomous`: how many turns the test may \
                            spend, at most 3. Omit to use the definition's own \
                            `max_turns`. Ignored for `heartbeat`, which is one prompt.",
                    },
                },
                "required": ["kind", "name"],
            }),
        };
        Self {
            state,
            workspace_root,
            spec,
        }
    }

    /// The permission row this run's turns are judged by — production's
    /// own, from `[autonomous] origin`.
    fn origin(&self) -> crate::tools::policy::Origin {
        match self.state.config.autonomous.origin {
            crate::config::AutonomousOrigin::Channel => crate::tools::policy::Origin::Channel,
            crate::config::AutonomousOrigin::Trusted => crate::tools::policy::Origin::Trusted,
        }
    }

    /// The namespace the test session lands in: the calling room profile's, so
    /// the report is readable where the operator already is. `None` is the
    /// lookup's answer on every path with neither a timer origin nor a
    /// task-local profile — voice-origin turns, turns that pin no profile
    /// (`/rpc`, A2A, unattended), and callers outside any turn at all,
    /// whether or not the gate could ever be reached. Display only, so this
    /// degrades to the default namespace rather than failing the report.
    fn namespace(&self) -> &str {
        match current_admin_room_profile_for(&self.state.config) {
            Some(name) => self.state.config.namespace_for_room_profile(&name),
            None => DEFAULT_NAMESPACE_NAME,
        }
    }

    /// A workspace-relative path, for an error message or a report.
    ///
    /// Display only, so it degrades rather than failing: a path this
    /// cannot strip shows up absolute instead of turning the message
    /// *about* a failure into a second failure. Same symlink caveat as
    /// [`ConfigTool::rel`] — it reports on the path it was handed, and
    /// `write_file` is where external resolution would happen.
    fn rel_display(&self, abs: &Path) -> String {
        slash_path(abs.strip_prefix(&self.workspace_root).unwrap_or(abs))
    }

    /// The session's own path, workspace-relative — the report's `Session:`
    /// line. Derived from the store rather than assembled from a template,
    /// so it cannot drift from the real file name.
    fn session_rel(&self, session_id: &str) -> String {
        let ns = self.namespace();
        self.state
            .autonomous_session_store
            .absolute_path_for(session_id)
            .map(|p| self.rel_display(&p))
            .unwrap_or_else(|| format!("sessions/{ns}/autonomous/{session_id}"))
    }

    fn load_heartbeat(&self, name: &str) -> Result<crate::heartbeat_config::HeartbeatTask> {
        let abs = definition_path(&self.workspace_root, "heartbeat", name)?;
        let raw = std::fs::read_to_string(&abs)
            .with_context(|| format!("failed to read {}", self.rel_display(&abs)))?;
        crate::heartbeat_config::parse_definition(name, &raw)
            .map_err(|e| anyhow!("cannot test {name}: {e}"))
    }

    fn load_autonomous(&self, name: &str) -> Result<crate::autonomous_config::AutonomousTask> {
        let abs = definition_path(&self.workspace_root, "autonomous", name)?;
        let raw = std::fs::read_to_string(&abs)
            .with_context(|| format!("failed to read {}", self.rel_display(&abs)))?;
        crate::autonomous_config::parse_definition(name, &raw)
            .map_err(|e| anyhow!("cannot test {name}: {e}"))
    }

    /// One turn, run exactly as production runs it.
    async fn turn(&self, session_id: &str, text: String) -> LlmTurnOutcome {
        crate::serve::run_llm_turn(
            Arc::clone(&self.state),
            session_id.to_string(),
            ChatMessage::user(text),
            Arc::new(AutonomousHost {
                origin: self.origin(),
            }),
            None,
        )
        .await
    }

    /// Create the test session, under the `test:` name and the caller's
    /// namespace.
    fn open_session(&self, kind: &str, name: &str) -> Result<String> {
        self.state
            .autonomous_session_store
            .create_autonomous_session(&test_room_id(kind, name), self.namespace())
            .with_context(|| format!("failed to create a test session for {name}"))
    }

    /// Close the session, then answer with what happened.
    ///
    /// Closed *before* the report is built, and closed even when the run
    /// failed: a session left open is the one state a later `session_list`
    /// would read wrongly, as a test still in progress.
    fn finish(
        &self,
        kind: &str,
        name: &str,
        session_id: &str,
        turns: usize,
        stop: &TurnStop,
        answer: Option<String>,
    ) -> Result<String> {
        if let Err(e) = self
            .state
            .autonomous_session_store
            .close_session(session_id)
        {
            warn!("task_test: cannot close the test session for {name}: {e}");
        }
        let answer = answer.unwrap_or_else(|| "(no answer)".to_string());
        Ok(format!(
            "{kind} task '{name}' ran {turns} turn(s) ({}).\nSession: {}\nResult: {answer}",
            describe_stop(stop),
            self.session_rel(session_id),
        ))
    }

    /// A heartbeat task is one prompt, so its test is exactly one turn —
    /// a heartbeat definition has no `max_turns` to honour anyway.
    async fn run_heartbeat(&self, name: &str) -> Result<String> {
        let task = self.load_heartbeat(name)?;
        let session_id = self.open_session("heartbeat", name)?;
        // The same prefix `Heartbeat::fire_task` puts in front of the body.
        let text = format!("[Heartbeat: {name}]\n\n{}", task.body);
        let outcome = self.turn(&session_id, text).await;
        self.finish(
            "heartbeat",
            name,
            &session_id,
            1,
            &outcome.stop,
            outcome.text,
        )
    }

    /// The autonomous test: production's own turn-1 assembly, then
    /// `CONTINUE_PROMPT` — so a run that works here works there.
    async fn run_autonomous(&self, name: &str, requested: Option<usize>) -> Result<String> {
        let task = self.load_autonomous(name)?;
        // `0` is the "not requested" sentinel; the definition's own cap is
        // still bounded, so a task declaring 50 turns cannot turn a test
        // into a long run.
        let cap = match capped_test_turns(requested) {
            0 => task.max_turns.min(MAX_TEST_TURNS),
            n => n,
        };
        let session_id = self.open_session("autonomous", name)?;
        let session_rel = self.session_rel(&session_id);

        let mut turns = 0;
        let mut last: Option<String> = None;
        // Overwritten by the first turn, which always runs: a parsed task's
        // `max_turns` is at least one and `cap` is therefore at least one.
        let mut stop = TurnStop::Replied;
        while turns < cap {
            let text = if turns == 0 {
                format!("{}\nSession: {session_rel}\n\n{}", marker(name), task.body)
            } else {
                format!("{}\n{CONTINUE_PROMPT}", marker(name))
            };
            let outcome = self.turn(&session_id, text).await;
            turns += 1;
            stop = outcome.stop;
            // Kept even when this turn produced no answer, so a run whose
            // final turn broke the provider still reports what the model
            // last said.
            match outcome.text {
                Some(text) => last = Some(text),
                None => break,
            }
            if last.as_deref().is_some_and(|t| t.trim() == "DONE") {
                break;
            }
        }

        self.finish("autonomous", name, &session_id, turns, &stop, last)
    }
}

#[async_trait]
impl Tool for TaskTestTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // Before the arguments are even read: the room-profile gate every
        // config tool shares. A run is the most powerful thing on this
        // surface — it drives a whole turn — so it is refused as firmly as a
        // write.
        if !current_admin_room_profile_for(&self.state.config)
            .is_some_and(|name| self.state.config.admin_allows_room_profile(&name))
        {
            anyhow::bail!(ROOM_REFUSAL);
        }
        let kind = input["kind"]
            .as_str()
            .context("missing 'kind' (\"heartbeat\" or \"autonomous\")")?;
        let name = input["name"]
            .as_str()
            .context("missing 'name' (the definition's file stem)")?;
        match kind {
            "heartbeat" => self.run_heartbeat(name).await,
            "autonomous" => {
                // Presence and type are different questions: a `max_turns`
                // that is there but not a non-negative integer is refused
                // rather than read as "omitted", because the caller asked
                // for a cap and would otherwise silently get the
                // definition's own.
                let requested = match input.get("max_turns") {
                    None => None,
                    Some(v) if v.is_null() => None,
                    Some(v) => Some(v.as_u64().with_context(|| {
                        format!("'max_turns' must be a non-negative integer, got {v}")
                    })? as usize),
                };
                self.run_autonomous(name, requested).await
            }
            other => anyhow::bail!(
                "task_test: unknown kind {other:?}. `kind` is \"heartbeat\" or \"autonomous\"."
            ),
        }
    }
}

/// Register the four admin tools when the deployment has named a room profile.
///
/// Extracted from `main.rs` so the condition is testable, because the
/// condition *is* the grant: with `[tools.admin].room_profiles` empty, `main`
/// must register nothing at all — four tools that always refuse would
/// still tell the model they exist, and "exists but never works" is a
/// worse answer than "does not exist". The run-time gate in
/// [`ConfigTool::gate`] and [`TaskTestTool::execute`] is the second half
/// of the same grant, for the transports that have no room of their own.
///
/// Called once `serve_state` exists, since `task_test` needs it.
pub(crate) async fn register_admin_tools(
    tool_set: &Arc<ToolSet>,
    workspace_root: &Path,
    config: Config,
    ws: Arc<WorkspaceState>,
    subagent: Option<&Arc<SubagentTool>>,
    serve_state: Arc<ServeState>,
) {
    if !config.config_tools_enabled() {
        return;
    }
    // `agent_config` refreshes the spec `ToolSet::specs` advertises, and
    // `ToolSet` owns those specs — so it holds a `Weak` to the very set
    // it is registered into. Same shape `RefreshSystemPromptTool` uses
    // for `Agent`, and the same reason: a strong ref would be a cycle.
    let tool_set_weak = Arc::downgrade(tool_set);
    for dir in ConfigDir::ALL {
        tool_set
            .register_tool(Box::new(ConfigTool::new(
                dir,
                workspace_root.to_path_buf(),
                config.clone(),
                Arc::clone(&ws),
                if dir == ConfigDir::Agents {
                    subagent.map(Arc::downgrade)
                } else {
                    None
                },
                tool_set_weak.clone(),
            )))
            .await;
    }
    tool_set
        .register_tool(Box::new(TaskTestTool::new(serve_state)))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatResponse;
    use crate::timer::{TimerOrigin, scope_timer_origin};
    use sapphire_framework::workspace::{AppContext, Workspace, WorkspaceState};
    use serde_json::json;
    use std::future::Future;

    /// A workspace with the three definition directories created, plus a
    /// `TempDir` the caller must hold on to for the files to survive.
    fn test_workspace() -> (tempfile::TempDir, PathBuf, Arc<WorkspaceState>) {
        static TEST_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();
        TEST_CTX.set_cache_dir(std::env::temp_dir().join("sapphire-agent-config-tools-test-cache"));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".sapphire-agent")).unwrap();
        for sub in ["heartbeat", "autonomous", "agents"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let ws = Workspace::from_root(&TEST_CTX, &root).unwrap();
        let state = Arc::new(WorkspaceState::open(ws).unwrap());
        (dir, root, state)
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        tool: ConfigTool,
    }

    /// The tool as the deployment wires it: `[tools.admin].room_profiles =
    /// ["ops"]`, `[room_profile.ops]` claiming `!ops:x`, the three
    /// directories present, no subagent / tool set back-reference (that is
    /// the agents-directory wiring, tested where it exists).
    fn fixture(dir: ConfigDir) -> Fixture {
        let (d, root, ws) = test_workspace();
        let mut config = Config::for_test();
        config.tools.admin.room_profiles = vec!["ops".to_string()];
        config.room_profiles.insert(
            "ops".to_string(),
            crate::config::RoomProfileConfig {
                profile: "dev".to_string(),
                rooms: vec!["!ops:x".to_string()],
                ..Default::default()
            },
        );
        let tool = ConfigTool::new(dir, root.clone(), config, ws, None, Weak::new());
        Fixture {
            _dir: d,
            root,
            tool,
        }
    }

    /// Every call goes through the same `TimerOrigin::Chat` scope the
    /// channel path uses, rather than a test-only hook: the gate is only
    /// worth testing on the path it actually reads.
    async fn in_room<F: Future>(room: &str, fut: F) -> F::Output {
        scope_timer_origin(
            TimerOrigin::Chat {
                room_id: room.to_string(),
            },
            fut,
        )
        .await
    }

    /// Every call goes through the ACP path's scope, the way an editor's
    /// prompt does: `run_llm_turn` scopes the profile the session pinned
    /// around tool execution, and no `TimerOrigin` is set on that path.
    async fn from_room_profile<F: Future>(profile: Option<&str>, fut: F) -> F::Output {
        scope_admin_room_profile(profile.map(str::to_string), fut).await
    }

    async fn call(tool: &ConfigTool, input: serde_json::Value) -> anyhow::Result<String> {
        in_room("!ops:x", tool.execute(&input)).await
    }

    #[test]
    fn definition_path_refuses_anything_but_a_bare_stem() {
        let root = Path::new("/ws");
        for bad in ["", ".", "..", "../etc/passwd", "a/b", "a\\b", ".hidden"] {
            assert!(
                definition_path(root, "heartbeat", bad).is_err(),
                "accepted {bad:?}"
            );
        }
        assert_eq!(
            definition_path(root, "heartbeat", "morning_call.md").unwrap(),
            Path::new("/ws/heartbeat/morning_call.md"),
            "a bare stem with the extension normalises to the same file"
        );
    }

    #[test]
    fn declared_enabled_defaults_to_true() {
        let body = "---\nschedule: \"0 8 * * *\"\n---\nBody\n";
        assert_eq!(declared_enabled(body), Some(true), "the loaders' default");
        assert_eq!(
            declared_enabled("---\nenabled: true\n---\nBody\n"),
            Some(true)
        );
        assert_eq!(
            declared_enabled("---\nenabled: false\n---\nBody\n"),
            Some(false)
        );
        assert_eq!(declared_enabled("# no frontmatter\n"), None);
        assert_eq!(
            declared_enabled("---\nenabled: yes please\n---\nBody\n"),
            None,
            "an unparseable value means the loader skips the file"
        );
    }

    #[tokio::test]
    async fn agent_config_has_no_set_enabled_action() {
        let f = fixture(ConfigDir::Agents);
        let err = call(
            &f.tool,
            json!({"action": "set_enabled", "name": "reviewer", "enabled": false}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("set_enabled"), "{err}");
        assert!(err.contains("agent_config"), "{err}");
    }

    #[tokio::test]
    async fn heartbeat_write_refuses_a_schedule_the_agent_cannot_read() {
        let f = fixture(ConfigDir::Heartbeat);
        let err = call(
            &f.tool,
            json!({
                "action": "write",
                "name": "morning",
                "content": "---\nschedule: \"every day at 8\"\n---\nWake up.\n"
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing to write"), "{err}");
        assert!(
            !f.root.join("heartbeat/morning.md").exists(),
            "a refused write must not leave a file behind"
        );
    }

    #[tokio::test]
    async fn autonomous_write_refuses_an_empty_body() {
        let f = fixture(ConfigDir::Autonomous);
        let err = call(
            &f.tool,
            json!({"action": "write", "name": "journal", "content": "---\n---\n\n"}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing to write"), "{err}");
        assert!(!f.root.join("autonomous/journal.md").exists());
    }

    #[tokio::test]
    async fn list_reports_names_and_effective_enabled() {
        let f = fixture(ConfigDir::Heartbeat);
        std::fs::write(
            f.root.join("heartbeat/off.md"),
            "---\nschedule: \"0 8 * * *\"\nenabled: false\n---\nOff.\n",
        )
        .unwrap();
        std::fs::write(
            f.root.join("heartbeat/on.md"),
            "---\nschedule: \"0 9 * * *\"\n---\nOn.\n",
        )
        .unwrap();

        let listing = call(&f.tool, json!({"action": "list"})).await.unwrap();
        assert!(listing.contains("off (disabled)"), "{listing}");
        assert!(listing.contains("on (enabled)"), "{listing}");

        // `read` hands back the file as it is, not a re-serialised form.
        let raw = call(&f.tool, json!({"action": "read", "name": "off"}))
            .await
            .unwrap();
        assert!(raw.starts_with("---\nschedule:"), "{raw}");
        assert!(raw.ends_with("Off.\n"), "{raw}");

        let empty = fixture(ConfigDir::Autonomous);
        assert_eq!(
            call(&empty.tool, json!({"action": "list"})).await.unwrap(),
            "No autonomous definitions."
        );
    }

    #[tokio::test]
    async fn set_enabled_keeps_the_rest_of_the_file() {
        let f = fixture(ConfigDir::Autonomous);
        let raw = "---\n# keep me\npriority: 10\nmax_turns: 2\n---\n\nJournal the day.\n";
        let created = call(
            &f.tool,
            json!({"action": "write", "name": "journal.md", "content": raw}),
        )
        .await
        .unwrap();
        assert!(created.contains("autonomous/journal.md"), "{created}");

        call(
            &f.tool,
            json!({"action": "set_enabled", "name": "journal", "enabled": false}),
        )
        .await
        .unwrap();

        let after = std::fs::read_to_string(f.root.join("autonomous/journal.md")).unwrap();
        assert!(after.contains("# keep me"), "{after}");
        assert!(after.contains("enabled: false"), "{after}");
        assert!(after.contains("priority: 10"), "{after}");
        assert!(after.ends_with("Journal the day.\n"), "{after}");
    }

    #[tokio::test]
    async fn set_enabled_refuses_a_file_without_frontmatter() {
        let f = fixture(ConfigDir::Heartbeat);
        let path = f.root.join("heartbeat/bare.md");
        std::fs::write(&path, "# Just markdown\n").unwrap();

        let err = call(
            &f.tool,
            json!({"action": "set_enabled", "name": "bare", "enabled": false}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("frontmatter"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Just markdown\n",
            "a file with nothing to flip is refused, not repaired"
        );
    }

    #[tokio::test]
    async fn every_action_is_refused_outside_an_allowed_room() {
        let f = fixture(ConfigDir::Heartbeat);
        for input in [
            json!({"action": "list"}),
            json!({"action": "read", "name": "morning"}),
            json!({
                "action": "write",
                "name": "morning",
                "content": "---\nschedule: \"0 8 * * *\"\n---\nHi\n"
            }),
            json!({"action": "set_enabled", "name": "morning", "enabled": false}),
        ] {
            let err = in_room("!random:z", f.tool.execute(&input))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("Permission denied"), "{input}: {err}");
        }

        // No scope at all is `/rpc`, `/acp` and voice — never a named room.
        let err = f
            .tool
            .execute(&json!({"action": "list"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Permission denied"), "{err}");
    }

    /// The ACP path: an editor's session carries the room profile its bearer
    /// token pinned, with no room id anywhere. Listing that profile is what
    /// lets `agent_config` manage subagent definitions from Zed — the whole
    /// reason this grant is no longer written in room ids.
    #[tokio::test]
    async fn an_acp_session_is_allowed_by_its_pinned_room_profile() {
        let f = fixture(ConfigDir::Agents);
        let listing = from_room_profile(Some("ops"), f.tool.execute(&json!({"action": "list"})))
            .await
            .unwrap();
        assert!(listing.contains("No agents definitions."), "{listing}");
    }

    /// Every other profile-shaped input refuses: a profile the operator did
    /// not list, and a turn with no profile at all (`/rpc`, A2A, the
    /// unattended loops).
    #[tokio::test]
    async fn an_acp_session_off_the_list_is_refused() {
        let f = fixture(ConfigDir::Agents);
        for profile in [Some("guest"), None] {
            let err = from_room_profile(profile, f.tool.execute(&json!({"action": "list"})))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("Permission denied"), "{profile:?}: {err}");
            assert!(err.contains("room_profiles"), "{profile:?}: {err}");
        }
    }

    /// A channel turn resolves its room to a profile the same way its
    /// provider does, so `[room_profile.default]` captures an unlisted room
    /// there exactly as it does for the provider. A room in a *different*
    /// profile than the allow list is a refusal.
    #[tokio::test]
    async fn a_channel_room_is_gated_by_its_resolved_room_profile() {
        let (d, root, ws) = test_workspace();
        let mut config = Config::for_test();
        config.tools.admin.room_profiles = vec!["ops".to_string()];
        for (name, room) in [("ops", "!ops:x"), ("guest", "!lounge:y")] {
            config.room_profiles.insert(
                name.to_string(),
                crate::config::RoomProfileConfig {
                    profile: "dev".to_string(),
                    rooms: vec![room.to_string()],
                    ..Default::default()
                },
            );
        }
        let tool = ConfigTool::new(ConfigDir::Agents, root, config, ws, None, Weak::new());

        in_room("!ops:x", tool.execute(&json!({"action": "list"})))
            .await
            .unwrap();
        let err = in_room("!lounge:y", tool.execute(&json!({"action": "list"})))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Permission denied"), "{err}");
        let _dir = d;
    }

    /// Voice is a device, not a room or an editor. It carries no profile a
    /// grant could name, and an `ops` allow list must not change that.
    #[tokio::test]
    async fn a_voice_turn_is_refused_even_when_a_profile_is_listed() {
        let f = fixture(ConfigDir::Agents);
        let err = scope_timer_origin(
            TimerOrigin::Voice {
                device_id: "speaker".to_string(),
            },
            f.tool.execute(&json!({"action": "list"})),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Permission denied"), "{err}");
    }

    /// The test session lands under the calling room profile's memory
    /// namespace, resolved from the profile rather than from a room id — so
    /// an ACP call writes its report where the operator already reads.
    #[tokio::test]
    async fn a_test_session_uses_the_room_profiles_namespace() {
        let mut state = ServeState::for_test_scripted(false, vec![test_response("ok")]);
        {
            let config = &mut Arc::get_mut(&mut state)
                .expect("uniquely owned immediately after construction")
                .config;
            config.tools.admin.room_profiles = vec!["ops".to_string()];
            config.room_profiles.insert(
                "ops".to_string(),
                crate::config::RoomProfileConfig {
                    profile: "dev".to_string(),
                    rooms: vec!["!ops:x".to_string()],
                    memory_namespace: Some("ops_ns".to_string()),
                    ..Default::default()
                },
            );
        }
        // A heartbeat test runs the real one-prompt path, which reads the
        // definition first — so the definition has to exist for the run to
        // reach the session at all.
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("heartbeat")).unwrap();
        std::fs::write(
            ws.join("heartbeat/x.md"),
            "---\nschedule: \"0 8 * * *\"\n---\nHeartbeat.\n",
        )
        .unwrap();
        let tool = TaskTestTool::new(Arc::clone(&state));
        from_room_profile(
            Some("ops"),
            tool.execute(&json!({"kind": "heartbeat", "name": "x"})),
        )
        .await
        .unwrap();
        // The session file, not only the report line: the namespace is what
        // routes it.
        let rows = state.autonomous_session_store.session_rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].meta.namespace.as_deref(), Some("ops_ns"));
    }

    #[tokio::test]
    async fn the_admin_tools_are_edits() {
        for dir in ConfigDir::ALL {
            let f = fixture(dir);
            assert_eq!(f.tool.kind(), ToolKind::Edit, "{:?}", dir.tool_name());
            assert_eq!(f.tool.spec().name, dir.tool_name());
        }
        assert_eq!(
            ConfigDir::ALL.map(|d| d.tool_name()),
            ["agent_config", "autonomous_config", "heartbeat_config"]
        );
    }

    /// The same shape `autonomous.rs`'s `mod tests` builds: one final
    /// answer, no tool calls.
    fn test_response(text: &str) -> ChatResponse {
        ChatResponse {
            prompt_usage: None,
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        }
    }

    /// A `ServeState` whose scripted provider answers with `responses`, with
    /// the `ops` room profile allow-listed so the tool's gate is passable.
    fn test_state(responses: Vec<ChatResponse>) -> Arc<ServeState> {
        let mut state = ServeState::for_test_scripted(false, responses);
        {
            let config = &mut Arc::get_mut(&mut state)
                .expect("uniquely owned immediately after construction")
                .config;
            config.tools.admin.room_profiles = vec!["ops".to_string()];
            config.room_profiles.insert(
                "ops".to_string(),
                crate::config::RoomProfileConfig {
                    profile: "dev".to_string(),
                    rooms: vec!["!ops:x".to_string()],
                    ..Default::default()
                },
            );
        }
        state
    }

    fn test_tool(responses: Vec<ChatResponse>) -> (Arc<ServeState>, TaskTestTool) {
        let state = test_state(responses);
        let tool = TaskTestTool::new(Arc::clone(&state));
        (state, tool)
    }

    /// The invariant the whole design hangs on: `autonomous::is_due`
    /// anchors a task's cooldown on the latest session whose `room_id` is
    /// the task's name, so a test session must never claim that name.
    #[test]
    fn a_test_session_does_not_claim_the_task_name() {
        assert_eq!(
            test_room_id("autonomous", "journal"),
            "test:autonomous:journal"
        );
        assert_ne!(test_room_id("autonomous", "journal"), "journal");
        assert_eq!(
            test_room_id("heartbeat", "morning"),
            "test:heartbeat:morning"
        );
    }

    /// `enabled: false` is the case this tool exists for: the task runs
    /// anyway, on the production prompt assembly, and the session it used
    /// is closed so a later `session_list` does not show a running test.
    #[tokio::test]
    async fn a_disabled_task_can_be_tested_and_the_session_is_closed() {
        let (state, tool) = test_tool(vec![test_response("working"), test_response("DONE")]);
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("autonomous")).unwrap();
        std::fs::write(
            ws.join("autonomous/journal.md"),
            "---\nenabled: false\n---\nWrite the journal.\n",
        )
        .unwrap();

        let out = in_room(
            "!ops:x",
            tool.execute(&json!({"kind": "autonomous", "name": "journal"})),
        )
        .await
        .unwrap();

        assert!(
            out.contains("autonomous task 'journal' ran 2 turn(s) (replied)"),
            "{out}"
        );
        // The model's own answer is what is reported — proving the turn
        // really ran rather than merely created a session.
        assert!(out.contains("Result: DONE"), "{out}");
        assert!(
            out.contains("Session: sessions/default/autonomous/"),
            "the report should name the session, workspace-relative: {out}"
        );

        let rows = state.autonomous_session_store.session_rows();
        assert_eq!(rows.len(), 1, "one test, one session");
        assert_eq!(rows[0].meta.room_id, "test:autonomous:journal");
        assert!(rows[0].is_closed, "a finished test must not look running");

        // The definition's body went into the first user message exactly
        // as production assembles it.
        let history = state
            .autonomous_session_store
            .load_session(&rows[0].meta.session_id)
            .unwrap();
        let first = match &history[0].parts[0] {
            crate::provider::ContentPart::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(first.starts_with("[Autonomous: journal]\n"), "{first}");
        assert!(first.ends_with("Write the journal.\n"), "{first}");
    }

    /// A test must never be a long run: the cap is what makes it a check
    /// on token appetite rather than a source of one.
    #[test]
    fn max_turns_is_capped_for_a_test() {
        assert_eq!(capped_test_turns(Some(50)), MAX_TEST_TURNS);
        assert_eq!(capped_test_turns(Some(MAX_TEST_TURNS)), MAX_TEST_TURNS);
        assert_eq!(capped_test_turns(Some(2)), 2);
        assert_eq!(capped_test_turns(Some(0)), 1, "zero would test nothing");
        assert_eq!(
            capped_test_turns(None),
            0,
            "`0` is the sentinel for \"use the definition's own max_turns\""
        );
    }

    /// Two answers are scripted; a one-turn run consumes only the first,
    /// which is what `Result: first` pins.
    #[tokio::test]
    async fn a_heartbeat_test_runs_exactly_one_turn() {
        let (state, tool) = test_tool(vec![test_response("first"), test_response("second")]);
        let ws = state.workspace.dir().to_path_buf();
        std::fs::create_dir_all(ws.join("heartbeat")).unwrap();
        std::fs::write(
            ws.join("heartbeat/morning.md"),
            "---\nschedule: \"0 8 * * *\"\n---\nWake up.\n",
        )
        .unwrap();

        let out = in_room(
            "!ops:x",
            tool.execute(&json!({"kind": "heartbeat", "name": "morning"})),
        )
        .await
        .unwrap();

        assert!(out.contains("1 turn(s) (replied)"), "{out}");
        assert!(out.contains("Result: first"), "{out}");
        assert!(!out.contains("second"), "a second turn was run: {out}");

        let rows = state.autonomous_session_store.session_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].meta.room_id, "test:heartbeat:morning");
        assert!(rows[0].is_closed);
    }

    /// The room gate comes first — before `kind` or `name` are read — and
    /// an unknown `kind` names one of two directories or is refused.
    #[tokio::test]
    async fn task_test_is_refused_without_a_chat_room() {
        let (_state, tool) = test_tool(vec![test_response("ok")]);

        // No `TimerOrigin::Chat` scope at all is `/rpc`, `/acp` and voice.
        let err = tool
            .execute(&json!({"kind": "autonomous", "name": "journal"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Permission denied"), "{err}");

        // A room is not the only thing that can be wrong: `kind` names one
        // of two definition directories, and anything else is refused
        // before a file is read.
        let err = in_room(
            "!ops:x",
            tool.execute(&json!({"kind": "agents", "name": "reviewer"})),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("heartbeat"), "{err}");

        // A `max_turns` that is present but not a non-negative integer is
        // refused, not read as "omitted" — a caller that asked for a cap
        // must not silently get the definition's own.
        for bad in [json!(-1), json!("5"), json!(3.5)] {
            let err = in_room(
                "!ops:x",
                tool.execute(&json!({"kind": "autonomous", "name": "journal", "max_turns": bad})),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("max_turns"), "{bad}: {err}");
            assert!(err.contains("non-negative integer"), "{bad}: {err}");
        }
    }
    /// A `rooms` that names nobody registers nothing — not four tools that
    /// always refuse. The difference matters: the model cannot ask for a
    /// tool it was never offered, and a refusal it can discover still tells
    /// it that this surface exists.
    #[tokio::test]
    async fn no_room_means_no_registration() {
        let (_dir, root, ws) = test_workspace();
        let serve_state = test_state(vec![test_response("ok")]);
        let set = Arc::new(ToolSet::new(Vec::new(), Vec::new()));
        let mut config = Config::for_test();
        config.tools.admin.room_profiles = Vec::new();
        assert!(!config.config_tools_enabled());

        register_admin_tools(&set, &root, config, ws, None, serve_state).await;

        assert!(
            set.specs_filtered(|_| true).await.is_empty(),
            "no room names one: none of the admin tools may be registered"
        );
        assert!(set.kinds().await.is_empty());
    }

    /// One room is all it takes, and it registers exactly four tools: the
    /// three `ConfigTool`s (`ConfigDir::ALL`) plus `task_test`. `kinds()`
    /// is checked for length as well as the specs are for their names, so a
    /// directory registered twice — or a spec pushed without its tool —
    /// fails here rather than at the first call.
    #[tokio::test]
    async fn one_room_registers_all_four() {
        let (_dir, root, ws) = test_workspace();
        let serve_state = test_state(vec![test_response("ok")]);
        let set = Arc::new(ToolSet::new(Vec::new(), Vec::new()));
        let mut config = Config::for_test();
        config.tools.admin.room_profiles = vec!["ops".to_string()];

        register_admin_tools(&set, &root, config, ws, None, serve_state).await;

        let mut names: Vec<String> = set
            .specs_filtered(|_| true)
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "agent_config",
                "autonomous_config",
                "heartbeat_config",
                "task_test",
            ]
        );

        let kinds = set.kinds().await;
        assert_eq!(kinds.len(), 4, "each name registered once: {kinds:?}");
        assert!(
            kinds.iter().all(|(_, kind)| *kind == ToolKind::Edit),
            "every admin tool is an edit: {kinds:?}"
        );
    }

    /// The seam the whole-branch review found untested: `after_write`
    /// refreshes both halves — `set_agents` (what `execute` dispatches
    /// from) and `replace_spec` (what the model is *offered*) — so a
    /// definition written while the agent runs is immediately
    /// advertised and callable, not one of the two. Asserted end to end:
    /// one `ToolSet`, one `Arc<SubagentTool>` in it, one `ConfigTool`
    /// wired to both by `Weak` exactly as `main` wires them.
    #[tokio::test]
    async fn a_written_agent_definition_is_offered_and_callable() {
        let (_dir, root, ws) = test_workspace();
        let subagent = Arc::new(SubagentTool::new(Vec::new()));
        let set = Arc::new(ToolSet::new(
            vec![Box::new(Arc::clone(&subagent)) as Box<dyn Tool>],
            Vec::new(),
        ));
        let mut config = Config::for_test();
        config.tools.admin.room_profiles = vec!["ops".to_string()];
        config.room_profiles.insert(
            "ops".to_string(),
            crate::config::RoomProfileConfig {
                profile: "dev".to_string(),
                rooms: vec!["!ops:x".to_string()],
                ..Default::default()
            },
        );
        let tool = ConfigTool::new(
            ConfigDir::Agents,
            root.clone(),
            config,
            ws,
            Some(Arc::downgrade(&subagent)),
            Arc::downgrade(&set),
        );

        // Nothing is advertised before the write: the set was built
        // around an empty definition list.
        let before = set
            .specs_filtered(|n| n == crate::tools::subagent::SUBAGENT_TOOL_NAME)
            .await;
        assert!(
            !before[0].description.contains("reviewer"),
            "{}",
            before[0].description
        );

        call(
            &tool,
            json!({
                "action": "write",
                "name": "reviewer",
                "content": "---
description: Reviews a diff.
---
You are a reviewer.
"
            }),
        )
        .await
        .unwrap();

        // (a) Offered: the spec the model is handed now lists it — this
        // is `replace_spec` having been reached, since `specs_filtered`
        // reads `ToolSet`'s own copy.
        let after = set
            .specs_filtered(|n| n == crate::tools::subagent::SUBAGENT_TOOL_NAME)
            .await;
        assert_eq!(after.len(), 1, "replace, not append");
        assert!(
            after[0].description.contains("reviewer"),
            "{}",
            after[0].description
        );
        assert!(
            after[0].description.contains("Reviews a diff."),
            "{}",
            after[0].description
        );

        // (b) Callable: a dispatch by that name resolves in the live
        // list and runs a real nested turn — this is `set_agents` having
        // been reached. A `TurnContext` is scoped by hand, the pattern
        // the subagent tests use, since `execute` only ever reads it
        // through `current_turn_context()`.
        let provider: Arc<dyn crate::provider::Provider> = Arc::new(
            crate::serve::StubProvider::new(vec![test_response("reviewed")]),
        );
        let state = ServeState::for_test_scripted(false, vec![test_response("unused")]);
        let ctx = Arc::new(crate::serve::TurnContext {
            state,
            provider: Arc::clone(&provider),
            progress: Arc::new(crate::serve::NullProgress),
            visible_specs: set.specs_filtered(|_| true).await.into(),
            timer_origin: None,
            session_id: None,
            subagent_depth: 0,
            admin_room_profile: None,
        });
        let out = crate::serve::scope_turn_context(
            ctx,
            in_room(
                "!ops:x",
                subagent.execute(&json!({"agent": "reviewer", "prompt": "Review this."})),
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("reviewed"), "{out}");
    }
}
