//! `skill`: load a written procedure — planning, TDD, debugging, code
//! review, finishing a branch — from the checkout on the editor's
//! machine that `crate::skills` resolves and indexes.
//!
//! Two things make this different from the other `client_*` tools:
//!
//! 1. **There is no ACP call to list a directory.** `create_terminal`
//!    is the only way to ask the editor's machine anything about its
//!    filesystem shape, so resolving and indexing the skills directory
//!    always goes through a shell script (`crate::skills::RESOLVE_AND_INDEX_SH`)
//!    run over `client_exec::run_client_command`, never `fs/read_text_file`.
//! 2. **Reading one skill's body prefers `fs/read_text_file` and falls
//!    back to the terminal.** An editor may scope `fs/read_text_file`
//!    to the open project, and the skills checkout is deliberately
//!    outside it — so the fallback (`cat` over the terminal) is the
//!    path expected to run in practice, not defensive padding.
//!
//! The resolved index is cached per session (see [`SkillTool::cache`]'s
//! doc) so a session's second and later `skill()` calls don't re-run
//! the resolver script.

use crate::provider::ToolSpec;
use crate::skills::{
    RESOLVE_AND_INDEX_SH, RESOLVE_OR_CREATE_SH, SkillEntry, SkillIndex, destination_name,
    parse_index, parse_resolved_dir, validate_entry_name, validate_source_url,
};
use crate::tools::acp_client::{AcpClient, ExitStatus, current_acp_client};
use crate::tools::client_exec::{ClientRun, run_client_command};
use crate::tools::client_tools::format_exit_status;
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The editor is not reachable: no ACP client is scoped to this call.
/// Worded the same as `client_tools::no_editor_error` on purpose — same
/// substring, same situation — but kept as its own copy rather than
/// made `pub(crate)` there, since this is the only other module that
/// needs it.
fn no_editor_error() -> anyhow::Error {
    anyhow::anyhow!("no editor is connected to this session; this tool only works over ACP")
}

/// The session key `SkillTool`'s cache (see its doc) is keyed on for
/// the turn currently executing. Shared by `skill`, `skill_install`,
/// `skill_update` and `skill_uninstall` so each derives it the same
/// way — `None` for a turn with no session to key on (no reachable
/// `TurnContext`, or a subagent's nested call, where
/// `TurnPersistence` is `None`), which the cache never writes under.
fn current_session_key() -> Option<String> {
    crate::serve::current_turn_context().and_then(|ctx| ctx.session_id.clone())
}

/// How long to wait for the client's shell to resolve and index the
/// skills directory, or to `cat` one skill's body when the
/// `fs/read_text_file` fallback is needed. Both run on the editor's
/// machine over a round trip this crate does not control the latency
/// of, so this is generous relative to what either operation actually
/// costs (a directory walk over a few files, or reading one of them).
const CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Crude cap on how many sessions' resolved indexes stay cached at
/// once. One entry costs a directory string plus a handful of skill
/// descriptions — a few hundred bytes — and losing one on eviction
/// costs only the next `skill()` call in that session a single
/// resolver round trip, so clearing the whole map past this size is
/// enough; nothing here needs LRU precision.
const MAX_CACHED_SESSIONS: usize = 128;

/// Load a skill: a written procedure for a kind of work — planning,
/// TDD, debugging, code review, finishing a branch — from a checkout
/// that lives on the editor's machine, not this agent's.
pub struct SkillTool {
    spec: ToolSpec,
    /// The resolved index, cached per session id.
    ///
    /// `SkillTool` is registered once into the `ToolSet` shared by
    /// every connection through `ServeState` (`src/main.rs`), and
    /// `/acp` accepts many concurrent editor connections against that
    /// one state. A single cache slot on the tool — what an earlier
    /// version of this code did, reasoning that "a session belongs to
    /// exactly one connection" made it safe — was wrong: that sentence
    /// says nothing about whether *this tool* is scoped per session,
    /// and it is not. Editor A calling `skill()` would resolve and
    /// cache machine A's directory; editor B, a different connection
    /// against the same shared `SkillTool`, would then be served A's
    /// listing and A's absolute paths — including A's home directory
    /// name — without the resolver ever running on B's machine. So
    /// this is a map keyed by session id, not one slot.
    ///
    /// The key comes from `TurnContext::session_id`
    /// (`src/serve/mod.rs`), reached via `current_turn_context()` in
    /// `execute` below. A turn with no session to key on — a
    /// subagent's nested tool call, where `TurnContext::session_id` is
    /// `None` because `TurnPersistence` is `None` for a subagent — is
    /// never cached: `resolve_index` always re-resolves and never
    /// writes to the map when its key is `None`. That is the safe
    /// default for an identity this code cannot establish: caching
    /// under some shared placeholder key would reopen the same
    /// cross-editor collision this map exists to close, if two
    /// different sessions' subagents both hit it.
    cache: Mutex<HashMap<String, Arc<SkillIndex>>>,
}

impl SkillTool {
    pub fn new() -> Self {
        Self {
            spec: build_spec(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Force the next `skill()` call *in `session_key`'s session* to
    /// re-resolve and re-index the directory instead of reusing the
    /// cached copy. Other sessions' cache entries are untouched.
    ///
    /// Called by `skill_install`/`skill_update`/`skill_uninstall` after
    /// any successful change to what is on disk — without this, a
    /// skill installed mid-session would stay invisible to `skill()`
    /// until the process restarted.
    pub fn invalidate_cache(&self, session_key: &str) {
        self.cache.lock().unwrap().remove(session_key);
    }

    /// Whether a finished client command's exit status counts as a
    /// failure worth reporting as one, rather than the command's
    /// output being trusted.
    ///
    /// Only a *known* bad outcome — an explicit non-zero exit code, or
    /// termination by signal — counts. `(exit_code: None, signal:
    /// None)` is treated as success rather than failure: it is the
    /// same "nothing to go on" state `client_tools::format_exit_status`
    /// already renders as `[exit status unknown]` rather than as an
    /// error elsewhere in this crate, and failing a call outright on
    /// an editor that simply didn't report a status would be a worse
    /// failure mode than trusting output that, in every case actually
    /// seen, was fine.
    fn command_failed(status: &ExitStatus) -> bool {
        status.signal.is_some() || matches!(status.exit_code, Some(code) if code != 0)
    }

    /// Resolve and index the skills directory for `session_key`'s
    /// session, or return that session's cached result from an earlier
    /// call. `session_key` of `None` (no session reachable — see
    /// `cache`'s doc) always re-resolves and is never written back.
    async fn resolve_index(
        &self,
        client: &Arc<dyn AcpClient>,
        session_key: Option<&str>,
    ) -> Result<Arc<SkillIndex>> {
        if let Some(key) = session_key
            && let Some(index) = self.cache.lock().unwrap().get(key).cloned()
        {
            return Ok(index);
        }

        let run = run_client_command(
            client,
            "sh",
            &["-c".to_string(), RESOLVE_AND_INDEX_SH.to_string()],
            None,
            CLIENT_TIMEOUT,
        )
        .await?;
        if run.timed_out_handle.is_some() {
            anyhow::bail!(
                "timed out resolving the skills directory on the editor's machine \
                 after {}s",
                CLIENT_TIMEOUT.as_secs()
            );
        }
        let status = run
            .status
            .expect("run_client_command always sets `status` when it does not time out");
        if Self::command_failed(&status) {
            anyhow::bail!(
                "resolving the skills directory failed on the editor's machine: {}",
                format_exit_status(&status).trim()
            );
        }
        let index = Arc::new(parse_index(&run.output.output)?);

        if let Some(key) = session_key {
            let mut cache = self.cache.lock().unwrap();
            if !cache.contains_key(key) && cache.len() >= MAX_CACHED_SESSIONS {
                cache.clear();
            }
            cache.insert(key.to_string(), Arc::clone(&index));
        }
        Ok(index)
    }

    /// Read one skill's body: `fs/read_text_file` first, falling back
    /// to `cat` over the terminal on *any* failure — not just ones
    /// that look like a scoping refusal. ACP gives no reliable way to
    /// tell "the editor refused because the path is out of project
    /// scope" apart from "the file doesn't exist" or a transient RPC
    /// error at this layer, and the cost of trying anyway is one
    /// wasted `cat` round trip. What makes trying unconditionally safe
    /// is the exit-status check below: `cat` failing on a path that is
    /// genuinely absent must be reported as a failure, not returned as
    /// an empty success.
    async fn load_body(client: &Arc<dyn AcpClient>, entry: &SkillEntry) -> Result<String> {
        if let Ok(body) = client.read_text_file(&entry.path, None, None).await {
            return Ok(body);
        }
        let run = run_client_command(
            client,
            "cat",
            std::slice::from_ref(&entry.path),
            None,
            CLIENT_TIMEOUT,
        )
        .await?;
        if run.timed_out_handle.is_some() {
            anyhow::bail!(
                "timed out reading {} on the editor's machine after {}s",
                entry.path,
                CLIENT_TIMEOUT.as_secs()
            );
        }
        let status = run
            .status
            .expect("run_client_command always sets `status` when it does not time out");
        if Self::command_failed(&status) {
            anyhow::bail!(
                "reading {} failed on the editor's machine: {}",
                entry.path,
                format_exit_status(&status).trim()
            );
        }
        Ok(run.output.output)
    }
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The directory a skill's `SKILL.md` lives in — everything up to the
/// last `/`. Skills reference siblings by relative path
/// (`./implementer-prompt.md`, `references/codex-tools.md`,
/// `scripts/task-brief`), so a response with a body but no directory
/// header leaves every one of those dead.
///
/// String splitting rather than `std::path::Path`: every path here
/// comes from a POSIX shell running on the client
/// (`RESOLVE_AND_INDEX_SH`), so it is always `/`-separated regardless
/// of what platform this agent process itself is compiled for.
fn skill_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(dir, _)| dir)
}

/// Render the index for a bare `skill()` call: every skill's name and
/// description, so the model can pick one without a second round trip.
fn format_list(index: &SkillIndex) -> String {
    if index.skills.is_empty() {
        return format!("No skills found in {}.", index.dir);
    }
    let mut out = format!("Skills available in {}:\n", index.dir);
    for skill in &index.skills {
        out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    out
}

fn build_spec() -> ToolSpec {
    ToolSpec {
        name: "skill".into(),
        description: "Load a skill: a written procedure for a kind of work — planning, TDD, \
            debugging, code review, finishing a branch. Call with no arguments to list \
            what is available; call with a name to load one.\n\n\
            Before any engineering or creative work — writing a plan, changing code, \
            debugging, reviewing — check this list first and follow a skill if one \
            applies. Skills reference sibling files by relative path; the response \
            names the skill's directory so those can be read."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill to load. Omit to list what is available."
                }
            }
        }),
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let client = current_acp_client().ok_or_else(no_editor_error)?;
        // See `cache`'s doc: `None` here (no turn context reachable, or
        // a subagent's nested call, which has a turn context but no
        // session behind it) means `resolve_index` re-resolves every
        // time rather than risking a shared cache slot.
        let session_key = current_session_key();
        let index = self.resolve_index(&client, session_key.as_deref()).await?;

        match input.get("name").and_then(|v| v.as_str()) {
            None | Some("") => Ok(format_list(&index)),
            Some(name) => {
                let Some(entry) = index.skills.iter().find(|s| s.name == name) else {
                    let known: Vec<&str> = index.skills.iter().map(|s| s.name.as_str()).collect();
                    anyhow::bail!("no skill named '{name}'. Available: {}", known.join(", "));
                };
                let body = Self::load_body(&client, entry).await?;
                Ok(format!(
                    "Skill directory: {}\n\n{body}",
                    skill_dir(&entry.path)
                ))
            }
        }
    }
}

/// Lets one `Arc<SkillTool>` back two independent `ToolSet` registrations
/// (the `skill` slot here and the `skill_install`/`skill_update`/
/// `skill_uninstall` slots below) that all read and invalidate the same
/// cache, rather than each `Box<dyn Tool>` owning its own disconnected
/// copy of `SkillTool`.
#[async_trait]
impl Tool for Arc<SkillTool> {
    fn kind(&self) -> ToolKind {
        (**self).kind()
    }

    fn spec(&self) -> &ToolSpec {
        (**self).spec()
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        (**self).execute(input).await
    }
}

// ---------------------------------------------------------------------------
// skill_install / skill_update / skill_uninstall
//
// These three are what actually changes the directory `skill` only ever
// reads. All the guard code below exists because of two facts that don't
// go away just because the URL or the name came from a model rather than
// a person: `git clone` against a model-chosen URL is an arbitrary-code
// delivery path (`ext::` URLs make git execute a command by design), and
// `skill_uninstall`'s `name` is a path component about to be joined onto
// a real directory and handed to `rm -rf`.
// ---------------------------------------------------------------------------

/// How long to wait for a local, non-network operation on the editor's
/// machine: resolving/creating the directory, checking `git status`, an
/// existence probe, `rm -rf`. Reuses `CLIENT_TIMEOUT` — these all cost
/// about as little as the reads `skill()` already waits `CLIENT_TIMEOUT`
/// for.
const LOCAL_TIMEOUT: Duration = CLIENT_TIMEOUT;

/// How long to wait for a `git` operation that talks to a remote:
/// `clone`, `pull`, `remote get-url`. Generous relative to what a small
/// skills checkout costs, because this is a round trip over whatever
/// network the editor's machine is on, not this crate's own latency.
const GIT_TIMEOUT: Duration = Duration::from_secs(180);

/// A finished (not timed-out, not failing-exit) [`ClientRun`], with the
/// command's own output kept separate from any release-terminal
/// warning.
///
/// Kept apart deliberately: a caller that tests `output` for meaning —
/// `skill_uninstall`'s dirty-checkout check, `skill_install`'s
/// existence probe — must see exactly the command's own text, not that
/// text with a release warning appended after it. A clean checkout
/// whose *unrelated* terminal-release call happened to fail must not
/// read as dirty just because the warning string was folded into the
/// same value the emptiness check runs against — which is what this
/// type replaced. `warning` is for callers building a message to show
/// the model, appended after the meaningful part of that message, not
/// before it.
struct FinishedRun {
    output: String,
    warning: Option<String>,
}

/// Interpret a finished [`ClientRun`]: bail on a timeout or a failing
/// exit status, otherwise return the command's output and any release
/// warning as a [`FinishedRun`]. `status` is `None` exactly when the
/// command timed out — checking that, rather than trusting whatever
/// output happened to arrive, is what Task 3 got wrong once already.
///
/// A failure's message includes the command's own output (trimmed)
/// alongside the exit status: an exit code alone ("failed: [exit code:
/// 128]") tells the model nothing about *why*, and every caller in this
/// file wants that reason surfaced, not just the number.
fn finish_run(run: ClientRun, context: &str, timeout: Duration) -> Result<FinishedRun> {
    if run.timed_out_handle.is_some() {
        anyhow::bail!(
            "timed out {context} on the editor's machine after {}s",
            timeout.as_secs()
        );
    }
    let status = run
        .status
        .expect("run_client_command always sets `status` when it does not time out");
    if SkillTool::command_failed(&status) {
        anyhow::bail!(
            "{context} failed on the editor's machine: {}\n{}",
            format_exit_status(&status).trim(),
            run.output.output.trim()
        );
    }
    Ok(FinishedRun {
        output: run.output.output,
        warning: run.release_warning,
    })
}

/// Append `warning` (if any) to `output` for display to the model —
/// the one place a [`FinishedRun`]'s two fields are recombined, kept
/// separate from every call site that instead needs `output` alone to
/// test its meaning. See [`FinishedRun`]'s doc.
fn with_warning(mut output: String, warning: Option<String>) -> String {
    if let Some(w) = warning {
        output.push('\n');
        output.push_str(&w);
    }
    output
}

/// Run `git <args>` on the editor's machine with credential prompting
/// disabled and its choice of transport pinned to `https`.
///
/// `AcpClient::create_terminal` has no environment parameter (confirmed
/// at `src/tools/acp_client.rs:91`), so both env vars below are
/// supplied by running `env` as the command instead of `git` directly:
///
/// - `GIT_TERMINAL_PROMPT=0` — without it, a repository needing
///   credentials prompts in a terminal the model cannot answer, and the
///   one-shot timeout becomes the only thing that ends the call: a
///   stall rather than a refusal.
/// - `GIT_ALLOW_PROTOCOL=https` — the one setting that closes every
///   config-based route to a non-https transport at once, rather than
///   the one `update_one`'s own remote check happens to string-match.
///   `git pull` with no repository argument does not necessarily
///   consult `remote.origin.url` at all: it resolves from
///   `branch.<current>.remote`, falling back to `origin` only when that
///   is unset — so a `.git/config` doctored with `[branch "main"]
///   remote = evil` and an `ext::` URL on `evil`, leaving
///   `remote.origin.url` honest, would pass that check and still reach
///   `git pull`. `url.<base>.insteadOf` is a second such path:
///   `remote get-url` reports the URL as stored, unrewritten, while the
///   rewrite happens later, at transport time. `GIT_ALLOW_PROTOCOL` is
///   enforced by git itself at the point it actually opens a transport,
///   so it catches both regardless of which config field carried the
///   bad URL.
///
/// **What this does not close:** `git pull` runs the checkout's own
/// `.git/hooks/post-merge` if the pull changed anything, and no URL or
/// protocol guard can prevent that. This is the person's own machine
/// and their own checkout, so a hook already sitting in it is inherent
/// to running `git pull` there at all — not a hole this tool opens —
/// but it is worth writing down rather than leaving for the next
/// reader to rediscover.
async fn run_git(
    client: &Arc<dyn AcpClient>,
    args: &[&str],
    timeout: Duration,
) -> Result<ClientRun> {
    let mut full: Vec<String> = Vec::with_capacity(args.len() + 3);
    full.push("GIT_TERMINAL_PROMPT=0".to_string());
    full.push("GIT_ALLOW_PROTOCOL=https".to_string());
    full.push("git".to_string());
    full.extend(args.iter().map(|s| s.to_string()));
    run_client_command(client, "env", &full, None, timeout).await
}

/// Single-quote `s` for embedding in a POSIX shell command line run on
/// the editor's machine. The resolved skills directory can contain
/// spaces (an `Application Support`/`Application Data`-shaped path is
/// the common case, not an edge one) and, in principle, an embedded
/// single quote — both are closed here rather than assumed away.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Resolve the skills directory on the editor's machine, creating it
/// only if none of the candidates already exists. Shared by all three
/// tools below — `skill_install` is the only one that can legitimately
/// be the first write, but what makes it safe for `skill_update` and
/// `skill_uninstall` to reuse the same call (rather than a second,
/// read-only resolver) is not that `mkdir -p` is idempotent — creation
/// only ever happens when nothing exists is the property that matters,
/// and it is what `RESOLVE_OR_CREATE_SH` walking candidates in the same
/// existence-first order as `RESOLVE_AND_INDEX_SH` guarantees. A
/// resolver that instead created under the first candidate whose base
/// variable was merely *set* — without testing for an existing
/// directory first — could disagree with the read-only resolver about
/// where an already-populated checkout lives, and `skill_update`/
/// `skill_uninstall` would then silently operate on a second, empty
/// directory beside the real one instead of refusing or finding it. See
/// `RESOLVE_OR_CREATE_SH`'s own doc in `crate::skills`.
async fn resolve_or_create_dir(client: &Arc<dyn AcpClient>) -> Result<String> {
    let run = run_client_command(
        client,
        "sh",
        &["-c".to_string(), RESOLVE_OR_CREATE_SH.to_string()],
        None,
        LOCAL_TIMEOUT,
    )
    .await?;
    let stdout = finish_run(run, "resolving the skills directory", LOCAL_TIMEOUT)?.output;
    let dir = parse_resolved_dir(&stdout)?;
    // A client answering a bare `SKILLS_DIR\t` line (no path after the
    // tab) parses successfully — `parse_resolved_dir` only requires the
    // line to exist — and would otherwise make every caller's `<dir>/
    // <name>` a path rooted at `/`. Deliberately not a leading-`/`
    // check: `$APPDATA`-derived paths are legitimately relative-looking
    // on the wire (`C:\Users\...\Roaming/sapphire-agent/skills`) and
    // must not be rejected here.
    if dir.is_empty() {
        anyhow::bail!("the editor's machine reported an empty skills directory");
    }
    Ok(dir)
}

/// The distinct top-level directory names an index's skills live under —
/// one per thing `skill_install` could have installed, whether that is a
/// single hand-written skill (`<dir>/<name>/SKILL.md`) or a bundle that
/// installs many skills under one git checkout
/// (`<dir>/<source>/skills/<name>/SKILL.md`). This is what
/// `skill_update` with no `name` updates: every checkout, not every
/// individual skill a checkout happens to contain.
fn top_level_entries(index: &SkillIndex) -> Vec<String> {
    let prefix = format!("{}/", index.dir);
    let mut names: Vec<String> = index
        .skills
        .iter()
        .filter_map(|s| {
            s.path
                .strip_prefix(&prefix)
                .and_then(|rest| rest.split('/').next())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// ---------------------------------------------------------------------------
// skill_install
// ---------------------------------------------------------------------------

/// Install a skill source: `git clone` a checkout onto the editor's
/// machine, into the skills directory `skill()` reads from.
pub struct SkillInstallTool {
    spec: ToolSpec,
    skill_tool: Arc<SkillTool>,
}

impl SkillInstallTool {
    pub fn new(skill_tool: Arc<SkillTool>) -> Self {
        Self {
            spec: build_install_spec(),
            skill_tool,
        }
    }
}

fn build_install_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_install".into(),
        description: "Install a skill source onto the editor's machine by cloning an \
            https:// git URL into the skills directory `skill()` reads from. Only plain \
            https:// URLs are accepted. Refuses if that source is already installed — use \
            skill_update instead."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "An https:// URL to clone, e.g. https://github.com/obra/superpowers"
                }
            },
            "required": ["url"]
        }),
    }
}

#[async_trait]
impl Tool for SkillInstallTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .context("missing 'url'")?;

        // Refused before any process starts: `validate_source_url` and
        // `destination_name` (which itself validates the derived name)
        // are pure checks against the string the model supplied, run
        // before this function ever looks for a client to talk to.
        validate_source_url(url)?;
        let name = destination_name(url)?;

        let client = current_acp_client().ok_or_else(no_editor_error)?;

        let dir = resolve_or_create_dir(&client).await?;
        let dest = format!("{dir}/{name}");

        // A directory-existence check ACP has no direct call for, done
        // as a probe whose exit code is always 0 (the `||` branch
        // covers "does not exist") so `finish_run`'s "nonzero exit is a
        // failure" reading isn't tripped by the ordinary case.
        let probe = run_client_command(
            &client,
            "sh",
            &[
                "-c".to_string(),
                format!(
                    "test -d {} && echo EXISTS || echo ABSENT",
                    shell_quote(&dest)
                ),
            ],
            None,
            LOCAL_TIMEOUT,
        )
        .await?;
        let probe_out = finish_run(
            probe,
            "checking whether the destination exists",
            LOCAL_TIMEOUT,
        )?
        .output;
        if probe_out.contains("EXISTS") {
            anyhow::bail!(
                "'{name}' is already installed at {dest}. Use skill_update to pull its \
                 latest changes, or skill_uninstall then skill_install to replace it."
            );
        }

        // The URL goes after `--` so it cannot be reparsed as an option
        // even if a check upstream were ever missed.
        let clone = run_git(
            &client,
            &["clone", "--depth", "1", "--", url, &dest],
            GIT_TIMEOUT,
        )
        .await?;
        let result = finish_run(clone, &format!("cloning {url}"), GIT_TIMEOUT)?;

        if let Some(key) = current_session_key() {
            self.skill_tool.invalidate_cache(&key);
        }

        Ok(with_warning(
            format!("Installed '{name}' to {dest}.\n{}", result.output),
            result.warning,
        ))
    }
}

// ---------------------------------------------------------------------------
// skill_update
// ---------------------------------------------------------------------------

/// Pull the latest changes for one installed skill source, or — with no
/// `name` — every source `skill()`'s index finds.
pub struct SkillUpdateTool {
    spec: ToolSpec,
    skill_tool: Arc<SkillTool>,
}

impl SkillUpdateTool {
    pub fn new(skill_tool: Arc<SkillTool>) -> Self {
        Self {
            spec: build_update_spec(),
            skill_tool,
        }
    }

    /// Update one entry: re-check its stored `origin` remote before
    /// pulling — `skill_install` only ever writes an `https` remote,
    /// but `.git/config` is an ordinary file on the person's machine.
    ///
    /// This is a cheap, specific sanity check, not what actually makes
    /// a doctored `.git/config` safe to pull from: `git pull` with no
    /// repository argument does not necessarily consult
    /// `remote.origin.url` at all (see `run_git`'s doc for
    /// `branch.<current>.remote` and `url.<base>.insteadOf`), so a
    /// config doctored through either of those would pass this check
    /// and still reach `git pull` with a bad transport. What actually
    /// closes that is `GIT_ALLOW_PROTOCOL=https`, set on every `git`
    /// invocation by `run_git` itself. What this check adds on top: a
    /// clear, specific refusal — naming the bad remote — for the common
    /// case where `origin` is what got doctored, rather than leaving
    /// that case to surface however `GIT_ALLOW_PROTOCOL` happens to
    /// fail the subsequent `git pull`.
    async fn update_one(client: &Arc<dyn AcpClient>, dir: &str, name: &str) -> Result<String> {
        validate_entry_name(name)?;
        let dest = format!("{dir}/{name}");

        let remote_run = run_git(
            client,
            &["-C", &dest, "remote", "get-url", "origin"],
            LOCAL_TIMEOUT,
        )
        .await?;
        let remote = finish_run(
            remote_run,
            &format!("checking {name}'s remote"),
            LOCAL_TIMEOUT,
        )?
        .output;
        validate_source_url(remote.trim())?;

        let pull_run = run_git(client, &["-C", &dest, "pull", "--ff-only"], GIT_TIMEOUT).await?;
        let result = finish_run(pull_run, &format!("updating {name}"), GIT_TIMEOUT)?;
        Ok(with_warning(result.output, result.warning))
    }
}

fn build_update_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_update".into(),
        description: "Pull the latest changes for a skill source installed with \
            skill_install. Omit `name` to update every installed source; one entry's \
            failure does not stop the others, and the result names every entry and what \
            happened to it."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The installed source to update. Omit to update everything installed."
                }
            }
        }),
    }
}

#[async_trait]
impl Tool for SkillUpdateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        // Validated before `current_acp_client()` is even consulted, so
        // this tool shares the "refuse before touching the client"
        // property `skill_install`/`skill_uninstall` already have. A
        // present-but-not-a-string `name` (`{"name": 123}`) must be
        // rejected outright rather than falling through `.as_str()`
        // returning `None` into the "no name" branch, which would
        // silently turn a malformed single-entry update into "update
        // everything installed" — the opposite of what was asked.
        // `null` is treated the same as an absent key: both mean
        // "omitted".
        let name = match input.get("name").filter(|v| !v.is_null()) {
            Some(v) => {
                let name = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "'name' must be a non-empty string, or omitted to update every \
                         installed source"
                    )
                })?;
                validate_entry_name(name)?;
                Some(name.to_string())
            }
            None => None,
        };

        let client = current_acp_client().ok_or_else(no_editor_error)?;

        match name {
            Some(name) => {
                let dir = resolve_or_create_dir(&client).await?;
                let result = Self::update_one(&client, &dir, &name).await?;
                if let Some(key) = current_session_key() {
                    self.skill_tool.invalidate_cache(&key);
                }
                Ok(format!("Updated '{name}':\n{result}"))
            }
            None => {
                let session_key = current_session_key();
                let index = self
                    .skill_tool
                    .resolve_index(&client, session_key.as_deref())
                    .await?;
                let names = top_level_entries(&index);
                if names.is_empty() {
                    return Ok(format!("No skill sources found in {}.", index.dir));
                }

                let mut lines = Vec::with_capacity(names.len());
                let mut any_ok = false;
                for name in &names {
                    match Self::update_one(&client, &index.dir, name).await {
                        Ok(msg) => {
                            any_ok = true;
                            lines.push(format!("{name}: {msg}"));
                        }
                        Err(e) => lines.push(format!("{name}: failed — {e}")),
                    }
                }

                if any_ok && let Some(key) = session_key {
                    self.skill_tool.invalidate_cache(&key);
                }

                Ok(lines.join("\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// skill_uninstall
// ---------------------------------------------------------------------------

/// Remove an installed skill source from the editor's machine.
///
/// There is no `force` override. A dirty checkout — uncommitted local
/// changes — is always refused, full stop; the person resolves it
/// themselves on their own machine, where their own uncommitted work
/// lives and `git stash`, a commit, or their own `rm -rf` are all one
/// command away. `ToolKind::Delete` is `Allow`ed rather than asked
/// about under `Origin::Acp(AcceptEdits)` (unlike `Execute`, which is
/// asked), so a model-settable `force` boolean here would have let the
/// model discard someone's uncommitted edits with nobody asked — and
/// uncommitted edits to a skill are exactly what a person would be
/// angriest to lose without being consulted.
pub struct SkillUninstallTool {
    spec: ToolSpec,
    skill_tool: Arc<SkillTool>,
}

impl SkillUninstallTool {
    pub fn new(skill_tool: Arc<SkillTool>) -> Self {
        Self {
            spec: build_uninstall_spec(),
            skill_tool,
        }
    }
}

fn build_uninstall_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_uninstall".into(),
        description: "Remove a skill source installed with skill_install from the editor's \
            machine. Always refuses if the checkout has local (uncommitted) changes — \
            there is no override. Tell the person to commit, stash, or remove it \
            themselves on their own machine if that happens."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The installed source to remove."
                }
            },
            "required": ["name"]
        }),
    }
}

#[async_trait]
impl Tool for SkillUninstallTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Delete
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .context("missing 'name'")?;
        // `skill_uninstall` takes a name, not a path: validated the same
        // way `skill_install`'s derived name is, and only ever joined
        // below to the resolver's own output — never to anything a
        // model could have supplied directly — so this cannot address
        // anything that is not a direct child of the skills directory.
        // Validated before `current_acp_client()`, so a bad name is
        // refused with no process started, the same as `skill_install`.
        validate_entry_name(name)?;

        let client = current_acp_client().ok_or_else(no_editor_error)?;

        let dir = resolve_or_create_dir(&client).await?;
        let dest = format!("{dir}/{name}");

        // One probe, always exiting 0 (each branch of the `if` prints
        // and nothing after it can fail), answers two questions ACP has
        // no direct call for: does `<dest>` exist at all, and — if so —
        // is it a git checkout `git status` can even be asked about. A
        // hand-written skill (`<dir>/<name>/SKILL.md` with no `.git`)
        // is a legitimate, installable entry that is simply not
        // version-controlled; treating `git status`'s "not a git
        // repository" failure as this tool's own failure would refuse
        // to remove it at all. Text-matching that failure's stderr was
        // the alternative and was rejected: it is locale-dependent, and
        // this probe answers both questions in one call without relying
        // on git's wording for either.
        let probe = run_client_command(
            &client,
            "sh",
            &[
                "-c".to_string(),
                format!(
                    "if [ ! -d {d} ]; then echo ABSENT; \
                     elif [ -d {d}/.git ]; then echo GITREPO; \
                     else echo PLAIN; fi",
                    d = shell_quote(&dest)
                ),
            ],
            None,
            LOCAL_TIMEOUT,
        )
        .await?;
        let probe_out =
            finish_run(probe, "checking whether the entry exists", LOCAL_TIMEOUT)?.output;

        if probe_out.contains("ABSENT") {
            anyhow::bail!("'{name}' is not installed at {dest}. Nothing to uninstall.");
        } else if probe_out.contains("GITREPO") {
            let status_run = run_git(
                &client,
                &["-C", &dest, "status", "--porcelain"],
                LOCAL_TIMEOUT,
            )
            .await?;
            // Deliberately *not* `finish_run`/`command_failed` here, even
            // though every other call in this file uses them. Those treat
            // an unreported exit status (`exit_code: None, signal: None`)
            // as success — the right default for a read, where the worst
            // case is trusting output that, in every case actually seen,
            // was fine. This call is the one thing standing between a
            // model-issued `skill_uninstall` and `rm -rf`: if the editor
            // never says whether `git status --porcelain` succeeded, "no
            // evidence of local changes" must not be read as "confirmed
            // no local changes." So an unreported status refuses here,
            // where `command_failed` would have let it through.
            if status_run.timed_out_handle.is_some() {
                anyhow::bail!(
                    "timed out checking {name} for local changes on the \
                     editor's machine after {}s",
                    LOCAL_TIMEOUT.as_secs()
                );
            }
            let status = status_run
                .status
                .expect("run_client_command always sets `status` when it does not time out");
            if status.exit_code.is_none() && status.signal.is_none() {
                anyhow::bail!(
                    "could not determine whether '{name}' at {dest} has local \
                     changes: the editor did not report an exit status for \
                     `git status --porcelain`. Refusing to remove it rather \
                     than guessing it is clean — try again, or check and \
                     remove it yourself on the editor's machine."
                );
            }
            if SkillTool::command_failed(&status) {
                anyhow::bail!(
                    "checking {name} for local changes failed on the editor's \
                     machine: {}\n{}",
                    format_exit_status(&status).trim(),
                    status_run.output.output.trim()
                );
            }
            if !status_run.output.output.trim().is_empty() {
                anyhow::bail!(
                    "'{name}' has local changes and was not removed:\n{}\n\
                     Resolve them on the editor's machine — commit, stash, or remove the \
                     directory yourself — then try again. There is no override.",
                    status_run.output.output.trim()
                );
            }
        } else if probe_out.contains("PLAIN") {
            // A hand-written entry with no `.git` directory at all — not
            // version-controlled, so there is no "local changes" question
            // to ask and nothing blocks removal.
        } else {
            // Neither ABSENT, GITREPO nor PLAIN: the editor answered the
            // existence probe with something this tool doesn't recognise
            // — most plausibly an empty string after a flaked round trip,
            // or a client that reports `exitStatus` without `exitCode`
            // (optional in the ACP schema) alongside truncated output.
            // Previously this fell straight through to `rm -rf` — an
            // unrecognised answer must instead be treated the same as
            // "cannot determine": a refusal, not a guess in the direction
            // of deleting something.
            anyhow::bail!(
                "could not determine whether '{name}' at {dest} exists or is \
                 a git checkout: the editor's answer to the existence probe \
                 was not recognised ({probe_out:?}). Refusing to remove it \
                 rather than guessing — try again, or check and remove it \
                 yourself on the editor's machine."
            );
        }

        // `--` so a name that were somehow still just a `-`-prefixed
        // string (validate_entry_name already refuses one) could not be
        // reparsed as an option, matching the clone's own `--`.
        let rm = run_client_command(
            &client,
            "rm",
            &["-rf".to_string(), "--".to_string(), dest.clone()],
            None,
            LOCAL_TIMEOUT,
        )
        .await?;
        let result = finish_run(rm, &format!("removing {name}"), LOCAL_TIMEOUT)?;

        if let Some(key) = current_session_key() {
            self.skill_tool.invalidate_cache(&key);
        }

        Ok(with_warning(
            format!("Uninstalled '{name}' from {dest}."),
            result.warning,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::acp_client::scope_acp_client;
    use crate::tools::acp_client::tests::FakeClient;

    /// One skill (`brainstorming`), in the bundled-source layout
    /// (`<dir>/<source>/skills/<name>/SKILL.md`) `RESOLVE_AND_INDEX_SH`
    /// also indexes.
    const INDEX_STDOUT: &str = "SKILLS_DIR\t/home/user/.local/share/sapphire-agent/skills\n\
        SKILL\t/home/user/.local/share/sapphire-agent/skills/bundle/skills/brainstorming/SKILL.md\n\
        FM\tname: brainstorming\n\
        FM\tdescription: You MUST use this before any creative work\n";

    fn fake_client_returning(stdout: &str) -> Arc<FakeClient> {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(stdout);
        client
    }

    /// A client whose `fs/read_text_file` always fails — as an editor
    /// that scopes it away from the skills checkout would — with the
    /// index resolver and the `cat` fallback each queued their own
    /// terminal stdout, since they are two separate `create_terminal`
    /// round trips.
    fn fake_client_where_fs_read_fails(index_stdout: &str, body: &str) -> Arc<FakeClient> {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(index_stdout);
        client.queue_terminal_stdout(body);
        *client.read_answer.lock().unwrap() =
            Some(Err("fs/read_text_file: out of scope".to_string()));
        client
    }

    async fn load_skill_with(client: Arc<FakeClient>, name: &str) -> Result<String> {
        let tool = SkillTool::new();
        scope_acp_client(client, async { tool.execute(&json!({"name": name})).await }).await
    }

    async fn load_skill(name: &str) -> Result<String> {
        load_skill_with(fake_client_returning(INDEX_STDOUT), name).await
    }

    /// Run a bare `skill()` call (list, or the index-resolution error)
    /// against `client`.
    async fn index_with(client: Arc<FakeClient>) -> Result<String> {
        let tool = SkillTool::new();
        scope_acp_client(client, async { tool.execute(&json!({})).await }).await
    }

    /// Two calls in the *same* session, one resolver invocation: the
    /// directory does not move during a session, and re-running a
    /// shell script per skill load would cost a round trip each time.
    /// Exercised directly against `resolve_index` (rather than through
    /// `execute`, which has no `TurnContext` to read a session id from
    /// in a bare unit test) — see `two_sessions_get_two_different_resolutions`
    /// for the regression test that pins the fix for Finding 1 (the
    /// cache used to have no session key at all).
    #[tokio::test]
    async fn the_index_is_resolved_once_and_reused_within_one_session() {
        let client = fake_client_returning(INDEX_STDOUT);
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        let tool = SkillTool::new();

        tool.resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();
        tool.resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();

        assert_eq!(client.terminal_count(), 1);
    }

    /// The regression test for Finding 1: `SkillTool` is registered
    /// once into the `ToolSet` shared by every `/acp` connection, so
    /// two different editors — two different sessions — must each get
    /// their own resolver run and their own directory, never one
    /// another's.
    #[tokio::test]
    async fn two_sessions_get_two_different_resolutions() {
        let client = Arc::new(FakeClient::default());
        client
            .queue_terminal_stdout("SKILLS_DIR\t/home/alice/.local/share/sapphire-agent/skills\n");
        client.queue_terminal_stdout("SKILLS_DIR\t/home/bob/.local/share/sapphire-agent/skills\n");
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        let tool = SkillTool::new();

        let alice = tool
            .resolve_index(&dyn_client, Some("session-alice"))
            .await
            .unwrap();
        let bob = tool
            .resolve_index(&dyn_client, Some("session-bob"))
            .await
            .unwrap();

        assert_eq!(alice.dir, "/home/alice/.local/share/sapphire-agent/skills");
        assert_eq!(bob.dir, "/home/bob/.local/share/sapphire-agent/skills");
        assert_ne!(alice.dir, bob.dir);
        assert_eq!(
            client.terminal_count(),
            2,
            "each session must trigger its own resolver run"
        );

        // And each session still gets its own cache: re-asking for
        // alice's session must not cost a third resolver run.
        let alice_again = tool
            .resolve_index(&dyn_client, Some("session-alice"))
            .await
            .unwrap();
        assert_eq!(alice_again.dir, alice.dir);
        assert_eq!(client.terminal_count(), 2);
    }

    /// A session id that never resolves to `Some` — the bare `execute`
    /// path outside any `TurnContext`, standing in for a subagent's
    /// nested call, where `TurnContext::session_id` is `None` — must
    /// never be cached: caching it under some shared placeholder key
    /// would reopen the same cross-session collision Finding 1 was
    /// about.
    #[tokio::test]
    async fn no_session_key_is_never_cached() {
        let client = fake_client_returning(INDEX_STDOUT);
        client.queue_terminal_stdout(INDEX_STDOUT);
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        let tool = SkillTool::new();

        tool.resolve_index(&dyn_client, None).await.unwrap();
        tool.resolve_index(&dyn_client, None).await.unwrap();

        assert_eq!(client.terminal_count(), 2);
    }

    /// Skills reference siblings by relative path — `./implementer-prompt.md`,
    /// `scripts/task-brief`. Without this header every one of those is dead.
    #[tokio::test]
    async fn loading_a_skill_prefixes_its_absolute_directory() {
        let out = load_skill("brainstorming").await.unwrap();
        assert!(
            out.contains("/skills/bundle/skills/brainstorming"),
            "no directory header in: {out}"
        );
    }

    /// An editor may scope `fs/read_text_file` to the open project, and
    /// the skills checkout is deliberately outside it. The fallback is
    /// the path we expect to take, not a belt-and-braces extra.
    #[tokio::test]
    async fn a_refused_fs_read_falls_back_to_the_terminal() {
        let client = fake_client_where_fs_read_fails(INDEX_STDOUT, "body from cat");
        let out = load_skill_with(client, "brainstorming").await.unwrap();
        assert!(out.contains("body from cat"), "got: {out}");
    }

    #[tokio::test]
    async fn an_unknown_name_lists_what_exists() {
        let out = load_skill("nope").await.unwrap_err().to_string();
        assert!(out.contains("brainstorming"), "got: {out}");
    }

    #[tokio::test]
    async fn no_directory_is_reported_with_the_fix() {
        let err = index_with(fake_client_returning("NO_SKILLS_DIR\n"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skill_install"));
    }

    /// A bare call with no editor's skill found still lists an empty
    /// result rather than panicking or erroring, when the directory
    /// resolves but has nothing in it.
    #[tokio::test]
    async fn an_empty_directory_lists_as_empty_not_an_error() {
        let out = index_with(fake_client_returning(
            "SKILLS_DIR\t/home/user/.local/share/sapphire-agent/skills\n",
        ))
        .await
        .unwrap();
        assert!(out.contains("No skills found"), "got: {out}");
    }

    #[tokio::test]
    async fn no_editor_refuses_rather_than_hanging() {
        let err = SkillTool::new()
            .execute(&json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no editor"), "got: {err}");
    }

    #[test]
    fn the_kind_is_read() {
        assert_eq!(SkillTool::new().kind(), ToolKind::Read);
    }

    /// `invalidate_cache` is Task 4's hook for making a freshly
    /// installed skill visible without a process restart — pinned here
    /// so a later task's use of it is against a proven contract: after
    /// invalidation, the next call for *that session* re-resolves
    /// rather than reusing the stale index.
    #[tokio::test]
    async fn invalidating_the_cache_forces_a_re_resolve() {
        let client = fake_client_returning(INDEX_STDOUT);
        client.queue_terminal_stdout(INDEX_STDOUT);
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        let tool = SkillTool::new();

        tool.resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();
        tool.invalidate_cache("session-a");
        tool.resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();

        assert_eq!(client.terminal_count(), 2);
    }

    /// `invalidate_cache` only clears the session named — a sibling
    /// session's cached entry (and resolver-call count) is untouched.
    #[tokio::test]
    async fn invalidating_one_session_leaves_another_cached() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(INDEX_STDOUT);
        client.queue_terminal_stdout(INDEX_STDOUT);
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        let tool = SkillTool::new();

        tool.resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();
        tool.resolve_index(&dyn_client, Some("session-b"))
            .await
            .unwrap();
        tool.invalidate_cache("session-a");
        // session-b's cache entry must still be warm.
        tool.resolve_index(&dyn_client, Some("session-b"))
            .await
            .unwrap();

        assert_eq!(client.terminal_count(), 2, "session-b must not re-resolve");
    }

    /// Finding 2: a `cat` fallback that runs but fails (the file
    /// genuinely doesn't exist, as opposed to `fs/read_text_file`
    /// merely being out of the editor's declared scope) must be
    /// reported as an error, not returned as an empty success.
    #[tokio::test]
    async fn a_failed_terminal_fallback_is_reported_not_swallowed() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(INDEX_STDOUT);
        client.queue_terminal_result("", Some(1));
        *client.read_answer.lock().unwrap() =
            Some(Err("fs/read_text_file: out of scope".to_string()));
        let dyn_client: Arc<dyn AcpClient> = client.clone();

        let tool = SkillTool::new();
        let err = scope_acp_client(dyn_client, async {
            tool.execute(&json!({"name": "brainstorming"})).await
        })
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("brainstorming"), "got: {err}");
        assert!(err.contains("exit code: 1"), "got: {err}");
    }

    /// Finding 3: the index resolver script itself can fail on the
    /// editor's machine (permission error, a broken `awk`, whatever) —
    /// that must be reported as the script having failed, not
    /// misattributed to `parse_index` finding no `SKILLS_DIR` line.
    #[tokio::test]
    async fn a_failed_resolver_script_is_reported_as_a_script_failure() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_result("some stderr leaked to stdout", Some(2));
        let dyn_client: Arc<dyn AcpClient> = client.clone();

        let tool = SkillTool::new();
        let err = tool
            .resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("failed"), "got: {err}");
        assert!(err.contains("exit code: 2"), "got: {err}");
    }

    /// An exit status the client simply didn't report
    /// (`exit_code: None, signal: None`, `FakeClient`'s default when
    /// nothing is queued) is treated as success, not failure — the
    /// same "nothing to go on" state `client_tools::format_exit_status`
    /// renders as `[exit status unknown]` elsewhere rather than as an
    /// error. `queue_terminal_stdout` already defaults to
    /// `exit_code: Some(0)`, so this pins the *other* non-failing case
    /// (no status reported at all) as still not an error.
    #[test]
    fn an_unreported_exit_status_does_not_count_as_failed() {
        assert!(!SkillTool::command_failed(&ExitStatus {
            exit_code: None,
            signal: None,
        }));
    }

    // -----------------------------------------------------------------
    // skill_install / skill_update / skill_uninstall
    // -----------------------------------------------------------------

    const RESOLVED_DIR_STDOUT: &str = "SKILLS_DIR\t/home/user/.local/share/sapphire-agent/skills\n";

    async fn install_with(client: Arc<FakeClient>, url: &str) -> Result<String> {
        let tool = SkillInstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(client, async { tool.execute(&json!({"url": url})).await }).await
    }

    /// A client that resolves the directory, then reports the
    /// destination as already present — so the tool must bail without
    /// ever reaching a third (`git clone`) call.
    async fn install_existing(url: &str) -> Result<String> {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("EXISTS\n");
        let tool = SkillInstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(client, async { tool.execute(&json!({"url": url})).await }).await
    }

    /// A client that resolves the directory, then answers the stored
    /// remote's `git remote get-url origin` with `remote` — used to
    /// drive `skill_update`'s pre-pull remote check without a `git
    /// pull` ever being reached when that remote is rejected.
    async fn update_where_remote_is(remote: &str) -> Result<String> {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout(&format!("{remote}\n"));
        let tool = SkillUpdateTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(client, async {
            tool.execute(&json!({"name": "superpowers"})).await
        })
        .await
    }

    /// Drives `skill_update` with no `name` against `entries`: each
    /// `(name, outcome)` becomes one top-level entry in the resolved
    /// index (so `skill_update` discovers it without a directory
    /// listing ACP cannot provide), a queued `remote get-url origin`
    /// answer of a valid https remote, and then either a successful
    /// `git pull` line or a failing one.
    async fn update_all_where(entries: &[(&str, Result<&str, &str>)]) -> Result<String> {
        let client = Arc::new(FakeClient::default());
        let mut index_stdout = String::from("SKILLS_DIR\t/skills\n");
        for (name, _) in entries {
            index_stdout.push_str(&format!(
                "SKILL\t/skills/{name}/SKILL.md\nFM\tname: {name}\nFM\tdescription: d\n"
            ));
        }
        client.queue_terminal_stdout(&index_stdout);
        for (_, outcome) in entries {
            client.queue_terminal_stdout("https://github.com/obra/superpowers\n");
            match outcome {
                Ok(text) => client.queue_terminal_stdout(text),
                Err(text) => client.queue_terminal_result(text, Some(1)),
            }
        }
        let tool = SkillUpdateTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(client, async { tool.execute(&json!({})).await }).await
    }

    /// A client that resolves the directory, answers the existence
    /// probe as an existing git checkout, then answers `git status
    /// --porcelain` with `status`, then (if `status` is clean) a
    /// successful `rm -rf`.
    async fn uninstall_where_status(status: &str) -> Result<String> {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("GITREPO\n");
        client.queue_terminal_stdout(status);
        client.queue_terminal_stdout("");
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(client, async {
            tool.execute(&json!({"name": "brainstorming"})).await
        })
        .await
    }

    /// No client scoped at all: a `name` that `validate_entry_name`
    /// rejects must fail before this tool ever looks for one — and
    /// `terminal_count() == 0` proves that, rather than just an `Err`
    /// that could equally come from "no editor is connected" (which a
    /// valid name would also see here, since no client is scoped).
    async fn uninstall(name: &str) -> (Result<String>, usize) {
        let client = Arc::new(FakeClient::default());
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        let result = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": name})).await
        })
        .await;
        (result, client.terminal_count())
    }

    #[tokio::test]
    async fn install_refuses_every_non_https_source_before_running_anything() {
        // `git clone` against an `ext::` URL executes a command, so this
        // must be refused without a process ever starting — assert on the
        // client having been asked for nothing, not just on the error.
        for bad in [
            "ext::sh -c evil",
            "file:///etc",
            "git@github.com:x/y",
            "--upload-pack=/bin/sh",
            "http://x/y",
        ] {
            let client = fake_client_returning("");
            let err = install_with(client.clone(), bad).await.unwrap_err();
            assert!(err.to_string().contains("https"), "{bad}: {err}");
            assert_eq!(client.terminal_count(), 0, "{bad} started a process");
        }
    }

    #[tokio::test]
    async fn install_refuses_a_source_that_is_already_present() {
        let err = install_existing("https://github.com/obra/superpowers")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skill_update"), "got: {err}");
    }

    #[tokio::test]
    async fn update_rejects_a_stored_remote_that_is_not_https() {
        // `skill_install` only ever writes an https remote, but
        // `.git/config` is an ordinary file on the person's machine and
        // `git pull` against an `ext::` remote executes a command.
        let err = update_where_remote_is("ext::sh -c evil").await.unwrap_err();
        assert!(err.to_string().contains("https"), "got: {err}");
    }

    #[tokio::test]
    async fn update_without_a_name_continues_past_one_failed_entry() {
        let out = update_all_where(&[
            ("a", Ok("Already up to date.")),
            ("b", Err("Not possible to fast-forward")),
            ("c", Ok("Updating 1234..5678")),
        ])
        .await
        .unwrap();
        // Substrings that pin success vs. failure per entry, not just
        // the entry's name — `out.contains("b")` alone would pass just
        // as happily whether "b" succeeded or failed, and would not
        // catch a regression that reported the wrong outcome for it.
        assert!(out.contains("a: Already up to date."), "{out}");
        assert!(out.contains("b: failed"), "{out}");
        assert!(out.contains("c: Updating 1234..5678"), "{out}");
    }

    /// No `force` override exists: a dirty checkout is always refused.
    /// The person resolves it themselves — `git stash`, a commit, or
    /// removing it by hand are all one command away on their own
    /// machine, where their uncommitted work actually is.
    #[tokio::test]
    async fn uninstall_refuses_a_checkout_with_local_changes() {
        let err = uninstall_where_status("M skills/brainstorming/SKILL.md")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SKILL.md"), "got: {err}");
    }

    #[tokio::test]
    async fn uninstall_will_not_address_anything_outside_the_skills_directory() {
        // Asserts on `terminal_count() == 0`, not just on an `Err` —
        // removing the `validate_entry_name` call at the top of
        // `execute` would still return an `Err` here (there is no
        // scoped client, so it would be "no editor is connected"
        // instead), and a test that only checked `is_err()` would not
        // catch that the guard itself was gone.
        for bad in ["..", "../../etc", "/etc", "a/b", "con"] {
            let (result, terminals) = uninstall(bad).await;
            assert!(result.is_err(), "accepted {bad}");
            assert_eq!(terminals, 0, "{bad} started a process");
        }
    }

    /// A malformed single-entry update (`name` present but not a
    /// string) must be rejected outright, not silently reinterpreted as
    /// "no name" and turned into an update-everything call — the
    /// opposite of what was asked. Also proves the client is never
    /// touched, matching the other two tools' "refuse before touching
    /// the client" property.
    #[tokio::test]
    async fn update_rejects_a_present_but_non_string_name() {
        let client = fake_client_returning("");
        let tool = SkillUpdateTool::new(Arc::new(SkillTool::new()));
        let err = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": 123})).await
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("string"), "got: {err}");
        assert_eq!(
            client.terminal_count(),
            0,
            "a malformed name started a process"
        );
    }

    /// `null` is treated the same as an absent `name`: an "update
    /// everything" call, not a rejection. Sends `{"name": null}`
    /// explicitly — routing through `update_all_where`, which calls
    /// `execute(&json!({}))` (no `name` key at all, not `name: null`),
    /// would leave this green even if the `.filter(|v| !v.is_null())`
    /// in `execute` were deleted, since an absent key never needed it.
    #[tokio::test]
    async fn update_treats_a_null_name_as_omitted() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(
            "SKILLS_DIR\t/skills\nSKILL\t/skills/a/SKILL.md\nFM\tname: a\nFM\tdescription: d\n",
        );
        client.queue_terminal_stdout("https://github.com/obra/superpowers\n");
        client.queue_terminal_stdout("Already up to date.");
        let tool = SkillUpdateTool::new(Arc::new(SkillTool::new()));
        let out = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": null})).await
        })
        .await
        .unwrap();
        assert!(out.contains("a: Already up to date."), "{out}");
    }

    /// `parse_resolved_dir` only requires the `SKILLS_DIR\t` line to
    /// exist, not that anything follows the tab — a client answering a
    /// bare line like that would otherwise make every `<dir>/<name>`
    /// path in this file resolve to `/<name>`, at the filesystem root.
    #[tokio::test]
    async fn resolve_or_create_dir_refuses_an_empty_resolved_directory() {
        let client = Arc::new(FakeClient::default());
        let dyn_client: Arc<dyn AcpClient> = client.clone();
        client.queue_terminal_stdout("SKILLS_DIR\t\n");
        let err = resolve_or_create_dir(&dyn_client).await.unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

    /// `Vec<&str>` -> `Vec<String>`, for comparing against
    /// `FakeClient.creates`' recorded argument vectors.
    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Pins the full command lines `skill_install` constructs — not
    /// just that some `Err`/`Ok` came back. A refactor that dropped the
    /// clone's `--` separator, or the `env` prefix that carries
    /// `GIT_TERMINAL_PROMPT=0`/`GIT_ALLOW_PROTOCOL=https`, would leave
    /// every other test in this file green; this is the one that would
    /// catch it.
    #[tokio::test]
    async fn install_pins_the_exact_argument_vectors() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("ABSENT\n");
        client.queue_terminal_stdout("Cloning into 'superpowers'...\ndone.");
        let tool = SkillInstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"url": "https://github.com/obra/superpowers"}))
                .await
        })
        .await
        .unwrap();

        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 3, "{creates:?}");
        assert_eq!(creates[0].0, "sh", "resolving the directory");
        assert_eq!(creates[1].0, "sh", "the existence probe");
        assert_eq!(
            creates[2].0, "env",
            "the clone, run through env for GIT_TERMINAL_PROMPT"
        );
        assert_eq!(
            creates[2].1,
            strs(&[
                "GIT_TERMINAL_PROMPT=0",
                "GIT_ALLOW_PROTOCOL=https",
                "git",
                "clone",
                "--depth",
                "1",
                "--",
                "https://github.com/obra/superpowers",
                "/home/user/.local/share/sapphire-agent/skills/superpowers",
            ])
        );
    }

    /// Same property as `install_pins_the_exact_argument_vectors`, for
    /// `skill_update`'s two `git` calls.
    #[tokio::test]
    async fn update_pins_the_exact_argument_vectors() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("https://github.com/obra/superpowers\n");
        client.queue_terminal_stdout("Already up to date.");
        let tool = SkillUpdateTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "superpowers"})).await
        })
        .await
        .unwrap();

        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 3, "{creates:?}");
        assert_eq!(
            creates[1].1,
            strs(&[
                "GIT_TERMINAL_PROMPT=0",
                "GIT_ALLOW_PROTOCOL=https",
                "git",
                "-C",
                "/home/user/.local/share/sapphire-agent/skills/superpowers",
                "remote",
                "get-url",
                "origin",
            ]),
            "the remote check"
        );
        assert_eq!(
            creates[2].1,
            strs(&[
                "GIT_TERMINAL_PROMPT=0",
                "GIT_ALLOW_PROTOCOL=https",
                "git",
                "-C",
                "/home/user/.local/share/sapphire-agent/skills/superpowers",
                "pull",
                "--ff-only",
            ]),
            "the pull"
        );
    }

    /// Same property again, for `skill_uninstall`'s probe, status check
    /// and `rm -rf --`.
    #[tokio::test]
    async fn uninstall_pins_the_exact_argument_vectors() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("GITREPO\n");
        client.queue_terminal_stdout("");
        client.queue_terminal_stdout("");
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "superpowers"})).await
        })
        .await
        .unwrap();

        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 4, "{creates:?}");
        assert_eq!(
            creates[2].1,
            strs(&[
                "GIT_TERMINAL_PROMPT=0",
                "GIT_ALLOW_PROTOCOL=https",
                "git",
                "-C",
                "/home/user/.local/share/sapphire-agent/skills/superpowers",
                "status",
                "--porcelain",
            ]),
            "the dirty check"
        );
        assert_eq!(creates[3].0, "rm");
        assert_eq!(
            creates[3].1,
            strs(&[
                "-rf",
                "--",
                "/home/user/.local/share/sapphire-agent/skills/superpowers",
            ]),
            "the removal"
        );
    }

    /// A hand-written entry (`<dir>/<name>/SKILL.md`, no `.git`) is not
    /// a git checkout at all, so there is no "local changes" question
    /// to ask — `git status` is never even invoked for it, and removal
    /// proceeds straight from the probe.
    #[tokio::test]
    async fn uninstall_removes_a_non_git_entry_without_a_status_check() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("PLAIN\n");
        client.queue_terminal_stdout("");
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "hand-written"})).await
        })
        .await
        .unwrap();

        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 3, "{creates:?}");
        assert_eq!(
            creates[2].0, "rm",
            "removal follows straight from the probe"
        );
    }

    /// An entry that does not exist at all gets a clear "not installed"
    /// refusal, not whatever `git status` on a nonexistent directory
    /// happens to print.
    #[tokio::test]
    async fn uninstall_reports_a_missing_entry_by_name() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("ABSENT\n");
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        let err = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "nope"})).await
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
        assert_eq!(client.terminal_count(), 2, "must not attempt rm -rf");
    }

    /// Blocking 2: the old code tested the existence probe's output with
    /// `contains("ABSENT")` / `contains("GITREPO")`, neither of which
    /// matches an empty string — the shape a flaked round trip or a
    /// client that never wrote anything would produce. That let control
    /// fall through both `if`s straight to `rm -rf`. An unrecognised
    /// answer must now refuse instead, before ever reaching `git status`
    /// or the removal itself.
    #[tokio::test]
    async fn uninstall_refuses_on_an_unrecognised_probe_answer() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("");
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        let err = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "superpowers"})).await
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not recognised"), "got: {err}");
        assert_eq!(
            client.terminal_count(),
            2,
            "must not fall through to git status or rm -rf on an \
             unrecognised probe answer"
        );
    }

    /// Blocking 2, the second way the guard failed open: `finish_run`/
    /// `command_failed` treat an unreported exit status (`exit_code:
    /// None, signal: None` — an ACP client that reports `exitStatus`
    /// without `exitCode`, which the protocol allows) as success. That
    /// is the right default for a read, but this call is what stands
    /// between a model-issued uninstall and `rm -rf`, so "the editor
    /// didn't say" must not be read as "confirmed clean."
    #[tokio::test]
    async fn uninstall_refuses_when_the_status_check_reports_no_exit_status() {
        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("GITREPO\n");
        client.queue_terminal_result("", None);
        let tool = SkillUninstallTool::new(Arc::new(SkillTool::new()));
        let err = scope_acp_client(Arc::clone(&client) as Arc<dyn AcpClient>, async {
            tool.execute(&json!({"name": "superpowers"})).await
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("could not determine"),
            "got: {err}"
        );
        assert_eq!(
            client.terminal_count(),
            3,
            "must refuse before ever running rm -rf"
        );
    }

    /// A `Provider` that is never actually called — it only exists to
    /// satisfy `TurnContext`'s field so a session id can be scoped for
    /// the cache-invalidation test below.
    struct UnusedProvider;
    #[async_trait]
    impl crate::provider::Provider for UnusedProvider {
        fn name(&self) -> &str {
            "unused"
        }
        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[crate::provider::ChatMessage],
            _tools: Option<&[ToolSpec]>,
        ) -> Result<crate::provider::ChatResponse> {
            anyhow::bail!("UnusedProvider::chat must never be called by this test")
        }
    }

    /// A minimal `TurnContext` scoped under `session_id`, so
    /// `current_session_key` (which `skill_install`/`skill_update`/
    /// `skill_uninstall` read to invalidate `SkillTool`'s cache) resolves
    /// to it. Every field but `session_id` is a stand-in never exercised
    /// by the test that uses this.
    fn test_turn_context(session_id: &str) -> Arc<crate::serve::TurnContext> {
        Arc::new(crate::serve::TurnContext {
            state: crate::serve::ServeState::for_test(true),
            provider: Arc::new(UnusedProvider),
            progress: Arc::new(crate::serve::NullProgress),
            visible_specs: Arc::from(Vec::<ToolSpec>::new()),
            timer_origin: None,
            session_id: Some(session_id.to_string()),
            subagent_depth: 0,
            admin_room_profile: None,
        })
    }

    /// A successful install must invalidate that session's cached
    /// index, or a freshly installed skill stays invisible to `skill()`
    /// for the rest of the session.
    #[tokio::test]
    async fn a_successful_install_invalidates_the_sessions_cache() {
        let skill_tool = Arc::new(SkillTool::new());
        let dyn_client: Arc<dyn AcpClient> = fake_client_returning(INDEX_STDOUT);
        skill_tool
            .resolve_index(&dyn_client, Some("session-a"))
            .await
            .unwrap();

        let client = Arc::new(FakeClient::default());
        client.queue_terminal_stdout(RESOLVED_DIR_STDOUT);
        client.queue_terminal_stdout("ABSENT\n");
        client.queue_terminal_stdout("Cloning into 'superpowers'...\ndone.");
        let install = SkillInstallTool::new(Arc::clone(&skill_tool));

        crate::serve::scope_turn_context(test_turn_context("session-a"), async {
            scope_acp_client(client, async {
                install
                    .execute(&json!({"url": "https://github.com/obra/superpowers"}))
                    .await
            })
            .await
        })
        .await
        .unwrap();

        // The cache must have been cleared: a fresh `resolve_index` call
        // hits the client again rather than reusing the stale copy.
        let client2 = fake_client_returning(INDEX_STDOUT);
        let dyn_client2: Arc<dyn AcpClient> = client2.clone();
        skill_tool
            .resolve_index(&dyn_client2, Some("session-a"))
            .await
            .unwrap();
        assert_eq!(client2.terminal_count(), 1);
    }

    #[test]
    fn the_kinds_are_set_deliberately() {
        assert_eq!(
            SkillInstallTool::new(Arc::new(SkillTool::new())).kind(),
            ToolKind::Execute
        );
        assert_eq!(
            SkillUpdateTool::new(Arc::new(SkillTool::new())).kind(),
            ToolKind::Execute
        );
        assert_eq!(
            SkillUninstallTool::new(Arc::new(SkillTool::new())).kind(),
            ToolKind::Delete
        );
    }
}
