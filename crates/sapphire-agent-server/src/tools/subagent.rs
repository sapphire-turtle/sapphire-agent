//! `subagent`: delegate a task to a specialised agent.
//!
//! A subagent runs a whole nested conversation — its own system prompt,
//! its own tool-calling loop, its own history — and hands back only its
//! final answer. Three properties make that safe rather than a loophole:
//!
//! 1. **Judged by the parent's `Origin`, through the parent's
//!    `TurnHost`.** The nested loop (`crate::serve::TurnLoop`) is handed
//!    the caller's own `progress` unchanged (see [`TurnContext`] in
//!    `src/serve/mod.rs`). If delegation ran under its own host or its
//!    own origin, "ask a subagent" would become a way to get done by
//!    proxy what the model was refused directly. (Three exceptions,
//!    each scoped narrowly: `turn_error`, `message_chunk`, and
//!    `round_budget` are *not* forwarded unchanged — see
//!    `SubagentTool::execute`'s host wrapper, `SubagentHost`,
//!    below.)
//! 2. **The system prompt is the definition and nothing else.** See
//!    [`subagent_system_prompt`].
//! 3. **The tool list actually offered is enforced, not just built
//!    restricted.** `subagent_tool_specs` builds each turn's list under
//!    the depth cap — past `tools.subagent.max_depth` it omits `subagent`
//!    entirely, so a turn at the cap is never *offered* delegation — but
//!    the list alone is a hint to the model, not a bound:
//!    `ToolSet::execute` dispatches by name across every tool the shared
//!    `ToolSet` has registered, `subagent` included, so nothing here
//!    would stop a turn from calling `subagent` by name even though its
//!    own list never offered it. What actually enforces the cap — and a
//!    definition's `tools:` restriction, and the same hallucinated-name
//!    gap on the parent's own turn — is `TurnLoop::run`'s permission gate
//!    refusing any call whose name is not in *that round's own*
//!    `tool_specs`, checked ahead of everything else including the
//!    host-access gate. The list this function builds and the gate's
//!    membership check are two halves of the one mechanism: only a spec
//!    this function *did* append is a `subagent` call the gate lets
//!    through. See `Refusal::NotOffered` in `crate::tools::policy`.
//!
//! `Tool::execute` receives only its JSON input — no session, no host,
//! no model. What a subagent needs to run is threaded through instead
//! via a `tokio::task_local` (`crate::serve::TurnContext`,
//! `scope_turn_context`/`current_turn_context`), the same vehicle
//! `crate::tools::acp_client` uses for the ACP connection.
//!
//! **What "isolation" does not cover.** `TurnHost::tool_start`/`tool_end`
//! fire on the *parent's* host for a subagent's own tool calls too (they
//! run inside the same `scope_turn_context`/`scope_memory_namespace`
//! wrapping every call in the parent's round), so a subagent's tool
//! activity is visible in the parent's ACP session stream as
//! notifications. That is necessary — it is what makes a permission
//! prompt for a subagent's call legible as coming from *this*
//! conversation — but it means the notification channel is not part of
//! what stays isolated. Nothing from it reaches the parent's stored
//! history or the ACP session store; only the returned final answer
//! does, as this tool's own result.
//!
//! **Lock re-entrancy is not a hazard here, deliberately.**
//! `SubagentTool::execute` runs *inside* `ToolSet::execute` (it is
//! itself one of the tools that set owns), and the nested loop it
//! drives calls back into the very same `ToolSet::execute` for its own
//! tool calls — `ToolSet::execute` is therefore entered twice,
//! re-entrantly, on the same task before the outer call returns. That
//! used to be a real hazard: `ToolSet::execute` held its read guard
//! across the whole call to `Tool::execute_full`, so for `subagent` the
//! guard was held across an entire nested conversation — up to
//! `[tools.tool_rounds]`'s `unattended` provider calls (a subagent is
//! always judged by that half of the budget, never `interactive`) plus
//! however long a human takes to answer an `AcpProgress::approve`
//! prompt. `tokio::sync::RwLock` is
//! task-fair: a reader blocks as soon as a writer is queued, so a
//! concurrent write (an MCP server's `tools/list_changed` refresh via
//! `ToolSet::refresh_if_needed`, or `mcp_reconnect`) queued behind that
//! held guard would then block the nested re-entrant read behind
//! *itself* — and every later call on every transport, since the
//! writer stays queued in front of them too. Nothing releases; the
//! agent stops answering until restart. `ToolSet::execute` now clones
//! the matched `Arc<dyn Tool>` under a short-lived read guard and drops
//! the guard before calling `execute_full`, so no execution — nested or
//! not — ever holds the lock. That removes the class of hazard rather
//! than this one instance of it, and as a side effect it is also what
//! stops `mcp_reconnect` from deadlocking on its own write lock while
//! its own call's read guard was still held (#201) — the same guard was
//! the cause of both.

use crate::agents::AgentDef;
use crate::provider::{ChatMessage, ToolSpec};
use crate::tools::{Tool, ToolKind};
use anyhow::Context;
use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, warn};

pub(crate) const SUBAGENT_TOOL_NAME: &str = "subagent";

/// Read `key` out of `input` as an optional string, distinguishing
/// "absent" (or explicit JSON `null`) from "present but not a string".
///
/// `agent` and `resume` used to go through a bare
/// `.get(key).and_then(|v| v.as_str())`, which conflates those two
/// cases: a non-string value quietly becomes `None`, the same as never
/// having been given at all. That is not just an unhelpful error
/// message — `{"agent": 123, "resume": "h", "prompt": "x"}` would slip
/// straight past the mutual-exclusivity check in `execute` and resume
/// silently, even though `agent` was very much present. Bailing here,
/// before that check ever runs, is what closes it.
fn string_field<'a>(input: &'a serde_json::Value, key: &str) -> anyhow::Result<Option<&'a str>> {
    match input.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => anyhow::bail!("'{key}' must be a string"),
    }
}

/// A subagent's whole system prompt: the definition's body, and
/// nothing else.
///
/// Not the workspace files (`SOUL.md`, `IDENTITY.md`, `USER.md`,
/// `AGENTS.md`, `TOOLS.md`), not a MEMORY.md digest, not the room
/// metadata, not the configured base prompt. Dropping those is not an
/// oversight, it is the feature: the main agent carries them
/// deliberately — it is someone to work *with* — and a code review does
/// not need yesterday's conversation. Inheriting them by default would
/// defeat the reason this exists.
///
/// The date used to be the one exception, on the grounds that an agent
/// which does not know today's date cannot use a tool that writes one.
/// It is a tool now (`current_time`), for the reason it stopped being
/// injected into the main agent's prompt: a timestamp in the prompt
/// text changes on every call, and a resumed subagent then re-processes
/// its whole conversation instead of hitting the provider's prompt
/// cache. A definition that restricts `tools:` and needs the date has
/// to list `current_time`; an unrestricted one inherits it.
///
/// This is a statement about the prompt, not about reach: an
/// unrestricted definition (`tools: None`) still inherits whichever
/// `memory_*` tools the parent can see, and — because
/// `SubagentTool::execute` reads `current_memory_namespace()` at call
/// time — those calls land in the same namespace the delegating
/// conversation is already in. A subagent is not told what is in
/// memory; it is not prevented from asking, unless its own `tools:`
/// list says so.
pub(crate) fn subagent_system_prompt(def: &AgentDef) -> String {
    def.prompt.clone()
}

/// The tools a subagent may use, and the `subagent` tool its own
/// delegations would show — see this module doc's third property for
/// what actually enforces the cap (the gate, not this list).
///
/// `def.tools` filters the inherited set exactly as before: `None` inherits
/// the parent's visible set, a list selects from it. The difference from
/// the pre-recursion behaviour is the `subagent` tool itself. It is still
/// stripped from the inherited list, but when the nested turn this list is
/// *for* is itself allowed to delegate — its own depth is `depth + 1`
/// (`depth` is the delegating turn's depth, main turn = 0) and it can
/// delegate exactly when that is still under the cap, `depth + 1 <
/// max_depth` — a *fresh* `subagent` spec is appended, carrying the agent
/// list the nested turn may itself delegate to.
/// When the depth cap forbids delegation, or `def.subagents` is `Some(
/// vec![])`, no `subagent` spec is appended, so the gate's membership check
/// refuses any `subagent` call and the turn cannot delegate.
///
/// The appended spec's agent list is derived the same way this function was
/// originally invoked on the delegator: `def.subagents == Some(list)`
/// rebuilds it narrowed to those names against `all_agents` (the currently
/// registered definitions); `None` inherits the delegator's own view verbatim
/// — the `subagent` spec already embedded in `parent_visible`, whose
/// description carries exactly the set that turn can see, is cloned
/// straight through. A delegator with no `subagent` spec in its own view can
/// only arise one level below the depth cap, already excluded by the
/// `depth + 1 < max_depth` check above, so there is no "no source spec to
/// clone" case to handle.
///
/// `ToolSpec.name` is `Cow<'static, str>`, so every comparison below goes
/// through `.as_ref()` rather than relying on a direct `Cow` vs `&str`
/// comparison.
pub(crate) fn subagent_tool_specs(
    def: &AgentDef,
    parent_visible: &[ToolSpec],
    all_agents: &[AgentDef],
    depth: u32,
    max_depth: u32,
) -> Vec<ToolSpec> {
    let nested_allowed = depth + 1 < max_depth && def.subagents != Some(vec![]);
    let mut specs: Vec<ToolSpec> = parent_visible
        .iter()
        .filter(|s| s.name.as_ref() != SUBAGENT_TOOL_NAME)
        // The client's per-session tools (MCP over ACP) belong to the main
        // turn; a delegate's loop has no session set to run them against.
        .filter(|s| !s.name.starts_with(crate::serve::SESSION_TOOL_PREFIX))
        // A delegate is not the character speaking, so it gets no avatar:
        // `SubagentHost` answers `avatar()` with `None` rather than share
        // the parent's (and with it the turn's one gesture), and a tool
        // that can only refuse is worse than absent. Stripped even when
        // `tools:` names one.
        .filter(|s| !crate::tools::avatar_tools::is_avatar_tool(s.name.as_ref()))
        .filter(|s| match &def.tools {
            Some(allowed) => allowed.iter().any(|a| a == s.name.as_ref()),
            None => true,
        })
        // A nested turn has no session to deliver a background command
        // into, so its `shell` is the one without `background`.
        .map(|s| {
            if s.name.as_ref() == "shell" {
                crate::tools::builtin_tools::shell_spec(false)
            } else {
                s.clone()
            }
        })
        .collect();
    if nested_allowed {
        let nested = match &def.subagents {
            // `None`: inherit the delegating turn's own view verbatim — the
            // spec already embedded there carries exactly the set it can see.
            // Less `background`, which only a main turn may use.
            None => parent_visible
                .iter()
                .find(|s| s.name.as_ref() == SUBAGENT_TOOL_NAME)
                .cloned()
                .map(without_background),
            // `Some(list)`: rebuild the spec narrowed to the listed names,
            // against the full registered list.
            Some(allowed) => {
                let visible: Vec<AgentDef> = all_agents
                    .iter()
                    .filter(|a| allowed.contains(&a.name))
                    .cloned()
                    .collect();
                Some(build_spec(&visible, false))
            }
        };
        specs.extend(nested);
    }
    specs
}

/// Build the tool's spec: a fixed preamble plus one line per agent, so
/// the parent model's only basis for choosing — each definition's own
/// `description` — actually reaches it.
///
/// A dispatched agent's answer is prefixed with a resumable handle (see
/// [`prefixed`]); passing that handle back as `resume` continues the
/// same child conversation — its own history, its own system prompt —
/// instead of starting a fresh one. `agent` and `resume` are mutually
/// exclusive: `SubagentTool::execute` rejects a call giving both, or
/// neither, naming the rule rather than picking a default.
///
/// `background` adds the `background` argument. Only the main turn's
/// spec carries it, and only while `[tools.background] max_per_session`
/// is above zero: a nested turn has no session to deliver into, so
/// `subagent_tool_specs` always builds its spec without it.
fn build_spec(agents: &[AgentDef], background: bool) -> ToolSpec {
    let mut description = String::from(
        "Delegate a task to a specialised agent, or continue one you already \
         started. A dispatched agent's own system prompt and its own \
         conversation are its own — only its final answer comes back — use \
         this to keep a large investigation out of this conversation. Its \
         answer is prefixed with a handle; pass that back as `resume` (with \
         a new `prompt`) to continue that same conversation instead of \
         starting a fresh one.\n\nAvailable agents:\n",
    );
    for agent in agents {
        description.push_str(&format!("- {}: {}\n", agent.name, agent.description));
    }
    if agents.is_empty() {
        // The heading is printed either way, so an empty list has to
        // say so in words — left as a dangling "Available agents:" it
        // reads as a rendering bug rather than as "none are configured
        // yet". Names the tool that fixes it, since the likeliest
        // reader is the model that is allowed to write one.
        description.push_str("(none yet \u{2014} create one with `agent_config` action `write`)\n");
    }

    let mut spec = ToolSpec {
        name: SUBAGENT_TOOL_NAME.into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Which agent to delegate to — one of the names \
                        listed in this tool's own description. Mutually \
                        exclusive with `resume`."
                },
                "resume": {
                    "type": "string",
                    "description": "A handle from an earlier delegation, to \
                        continue that subagent's own conversation. Mutually \
                        exclusive with `agent`."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task, or the next instruction for a \
                        resumed subagent. It sees only this (plus its own \
                        prior history, when resuming) and its own \
                        definition — nothing else from the current \
                        conversation."
                }
            },
            "required": ["prompt"]
        }),
    };
    if background && let Some(properties) = spec.input_schema["properties"].as_object_mut() {
        properties.insert(
            "background".to_string(),
            json!({
                "type": "boolean",
                "description": "Run it in the background and return at once; the result is delivered to this conversation when it finishes. Main conversation only."
            }),
        );
    }
    spec
}

/// `spec` without its `background` argument — what a nested turn
/// inherits from its delegator's spec.
fn without_background(mut spec: ToolSpec) -> ToolSpec {
    if let Some(properties) = spec.input_schema["properties"].as_object_mut() {
        properties.remove("background");
    }
    spec
}

/// Forwards every `TurnHost` method to the parent's host except three:
/// `turn_error`, `message_chunk`, and `round_budget`, each special-cased
/// for its own reason (see that method's doc below).
///
/// The name says what the type *is* — the host a subagent's nested turn
/// runs under, mostly the parent's own — rather than naming any one of
/// its exceptions, now that there are three of them.
///
/// A subagent's nested `TurnLoop` runs with `progress: &ctx.progress` —
/// the parent's own host, by design (see the module doc's first
/// property). But `progress.turn_error` is not part of that judgement;
/// it is how a turn reports itself terminally failed to whoever is
/// waiting on *this* request id. For `/rpc` and voice, `SseProgress`'s
/// impl sends a terminal JSON-RPC error carrying the parent's own
/// `req_id` the instant it is called — so a subagent's own provider
/// failure, left unwrapped, would fire that mid-turn while the parent
/// turn is still running, and the parent would go on to send its own
/// terminal response for the same id: two terminal frames for one
/// request, which `run_turn` (`src/serve/mod.rs`) assumes cannot
/// happen. On ACP the effect is milder (`AcpProgress::turn_error` only
/// records a message, read back solely when the *parent's* own turn
/// ends with no reply) but still wrong: a subagent's failure has no
/// business overwriting what the parent's own failure, if any, would
/// have said. Swallowing it here only keeps a subagent's provider failure
/// off *that* channel — it does not, on its own, keep the cause from the
/// model: `TurnStop::ProviderError` carries its own `message` now (the
/// same text `run_llm_turn`, `src/serve/mod.rs`, logs via
/// `error!("Provider error: {e:#}")` and hands to `turn_error` before this
/// wrapper swallows it), and `run_and_store` (below) reads it off the
/// variant and returns [`provider_error`]'s `Err` instead of ever calling
/// `answer_text`. So a subagent's own provider failure — an upstream rate
/// limit, insufficient API credit, a network error — reaches the parent
/// model as the `subagent` tool call's own error, distinguishable from a
/// subagent that simply produced no answer, even though it never reaches
/// the parent's own terminal-response channel.
///
/// Every other method is forwarded completely unchanged: `origin()`,
/// `approve()`, `standing_approval()`, `declined_refusal()`,
/// `acp_client()`, `client_fs_caps()`,
/// `client_terminal_cap()`, `tool_start`/`tool_end`/`tool_allowed` all
/// still resolve to the parent's own host, because those are what keep
/// delegation inside the same permission gate — `turn_error`,
/// `message_chunk`, and `round_budget` are the three special-cased.
///
/// `acp_client()` is forwarded for that same reason and one more: a
/// delegated child runs *where its parent runs*. On an ACP turn that
/// machine is the editor's, so a child's `file_read`/`file_write`/
/// `shell` calls must reach that same editor — the point of the
/// unified tool names (#270) — rather than falling back to the agent's
/// own disk, which is exactly what a `None` here would silently select.
/// `TurnLoop::run` is what scopes the client as a task-local around each
/// tool call, and the nested turn is awaited inside that scope, so the
/// child's tools inherit it; see
/// `a_delegated_subagents_tool_calls_still_reach_the_editor`.
struct SubagentHost(std::sync::Arc<dyn crate::serve::TurnHost>);

#[async_trait]
impl crate::serve::TurnHost for SubagentHost {
    async fn tool_start(&self, id: &str, name: &str, input: &serde_json::Value) {
        self.0.tool_start(id, name, input).await;
    }

    async fn tool_end(&self, id: &str, name: &str) {
        self.0.tool_end(id, name).await;
    }

    /// Swallowed — see the type doc. Nothing is lost operationally (the
    /// cause is logged before this would have fired); it just does not
    /// also masquerade as *this* turn's terminal outcome, and — per the
    /// type doc — it does not reach the parent model either.
    async fn turn_error(&self, _message: &str) {}

    /// Swallowed, like `turn_error` and for a related reason: a
    /// subagent's prose is not the parent agent's speech. Forwarding it
    /// would put the delegate's narration into the editor under the
    /// parent's name, which misattributes it — and the parent has no way
    /// to correct the record, because by the time it sees the subagent's
    /// answer the chunks are already on screen.
    ///
    /// Nothing is lost: what the subagent concluded comes back as the
    /// `subagent` tool's result, which is where the parent reads it and
    /// where the user sees it attributed correctly.
    ///
    /// `tool_start`/`tool_end` still forward, deliberately — see the
    /// module doc. Those are what make a permission prompt for a
    /// subagent's call legible as coming from *this* session.
    async fn message_chunk(&self, _text: &str) {}

    /// Not delegated, and this one must not be: a nested turn under an
    /// unbounded parent would itself be unbounded, so a parent that
    /// delegates in a loop would have no cap anywhere. A subagent is
    /// always judged `unattended` — nobody can cancel it directly, only
    /// the whole parent turn — whatever route the parent came in on.
    fn round_budget(&self) -> crate::serve::RoundBudget {
        crate::serve::RoundBudget::Unattended
    }

    fn origin(&self) -> crate::tools::policy::Origin {
        self.0.origin()
    }

    fn acp_client(&self) -> Option<std::sync::Arc<dyn crate::tools::acp_client::AcpClient>> {
        self.0.acp_client()
    }

    /// Forwarded, like `acp_client`: a subagent delegated from an ACP
    /// session works in the same session cwd, so its system prompt carries
    /// the same `# Current Workspace` block as its parent's.
    fn cwd(&self) -> Option<String> {
        self.0.cwd()
    }

    fn client_fs_caps(&self) -> (bool, bool) {
        self.0.client_fs_caps()
    }

    fn client_terminal_cap(&self) -> bool {
        self.0.client_terminal_cap()
    }

    async fn tool_allowed(&self, id: &str) {
        self.0.tool_allowed(id).await;
    }

    async fn approve(
        &self,
        call: &crate::provider::ToolCall,
        kind: ToolKind,
    ) -> crate::tools::policy::Approval {
        self.0.approve(call, kind).await
    }

    async fn standing_approval(
        &self,
        call: &crate::provider::ToolCall,
        kind: ToolKind,
    ) -> Option<crate::tools::policy::Approval> {
        self.0.standing_approval(call, kind).await
    }

    /// Forwarded with `approve`: whoever answered says why it was no.
    /// Under a background subagent that is nobody, and the nested
    /// child's refusal must not claim the user declined.
    fn declined_refusal(
        &self,
        approval: crate::tools::policy::Approval,
    ) -> crate::tools::policy::Refusal {
        self.0.declined_refusal(approval)
    }
}

/// The host a background subagent's nested turn runs under.
///
/// The parent's turn has usually ended by the time the child calls a
/// tool, so nobody is there to answer a permission dialog. The
/// permission table stays the parent's own — `origin()` is forwarded
/// unchanged, so a background child cannot run what its parent would
/// be refused — and only the `Ask` row changes meaning: `approve` never
/// asks. It takes the editor's standing answer (through the parent's
/// `standing_approval`) and refuses everything without one, recording
/// the call in `refused` so the completion report can tell the user
/// what is waiting on them. A standing "always reject" is not recorded:
/// the user already said never, and is not waiting to be asked. Falling
/// through to the parent's `approve` instead would put a dialog in
/// front of the user that is not about the turn they are looking at,
/// or, on a host whose `approve` defaults to `AllowOnce`, run the call
/// with nobody's say-so.
///
/// `tool_start` / `tool_end` / `tool_allowed` are logged, not
/// forwarded. `SubagentHost` forwards them to make a permission prompt
/// legible as coming from this session; with no prompt, all they would
/// do is push tool-call updates for calls that belong to no turn the
/// editor is showing, which can confuse it.
///
/// `turn_error` and `message_chunk` are swallowed and `round_budget` is
/// `Unattended`, for the reasons `SubagentHost` gives. `acp_client`,
/// `cwd` and the client capabilities are forwarded: the child works on
/// the editor's machine while the connection lasts, and its client-side
/// tools fail once it is gone, which the child reads and reports.
/// `avatar` is `None`: a background child has no gesture to make in
/// the parent's turn.
pub(crate) struct BackgroundSubagentHost {
    parent: Arc<dyn crate::serve::TurnHost>,
    refused: Arc<std::sync::Mutex<Vec<crate::background::RefusedCall>>>,
    /// The child's own handle, which names it when it asks a question.
    handle: String,
}

impl BackgroundSubagentHost {
    /// `refused` is shared so the caller can read it after the child's
    /// turn ends.
    pub(crate) fn new(
        parent: Arc<dyn crate::serve::TurnHost>,
        refused: Arc<std::sync::Mutex<Vec<crate::background::RefusedCall>>>,
        handle: String,
    ) -> Self {
        Self {
            parent,
            refused,
            handle,
        }
    }
}

#[async_trait]
impl crate::serve::TurnHost for BackgroundSubagentHost {
    async fn tool_start(&self, id: &str, name: &str, _input: &serde_json::Value) {
        debug!("background subagent: tool call {id} ({name}) started");
    }

    async fn tool_end(&self, id: &str, name: &str) {
        debug!("background subagent: tool call {id} ({name}) ended");
    }

    async fn turn_error(&self, _message: &str) {}

    async fn message_chunk(&self, _text: &str) {}

    fn round_budget(&self) -> crate::serve::RoundBudget {
        crate::serve::RoundBudget::Unattended
    }

    fn origin(&self) -> crate::tools::policy::Origin {
        self.parent.origin()
    }

    fn acp_client(&self) -> Option<Arc<dyn crate::tools::acp_client::AcpClient>> {
        self.parent.acp_client()
    }

    fn cwd(&self) -> Option<String> {
        self.parent.cwd()
    }

    fn client_fs_caps(&self) -> (bool, bool) {
        self.parent.client_fs_caps()
    }

    fn client_terminal_cap(&self) -> bool {
        self.parent.client_terminal_cap()
    }

    fn avatar(&self) -> Option<Arc<crate::tools::avatar_tools::AvatarTurn>> {
        None
    }

    /// The parent's identity, with this child named as the one asking, so
    /// the answer goes to the session that started it.
    fn consult_identity(&self) -> Option<crate::serve::ConsultIdentity> {
        self.parent
            .consult_identity()
            .map(|id| crate::serve::ConsultIdentity {
                asked_by: Some(self.handle.clone()),
                ..id
            })
    }

    async fn tool_allowed(&self, id: &str) {
        debug!("background subagent: tool call {id} allowed");
    }

    /// Never asks — see the type doc.
    async fn approve(
        &self,
        call: &crate::provider::ToolCall,
        kind: ToolKind,
    ) -> crate::tools::policy::Approval {
        use crate::tools::policy::Approval;
        if let Some(standing) = self.parent.standing_approval(call, kind).await {
            return standing;
        }
        self.refused
            .lock()
            .unwrap()
            .push(crate::background::RefusedCall {
                name: call.name.clone(),
                input: call.input.clone(),
            });
        Approval::RejectOnce
    }

    async fn standing_approval(
        &self,
        call: &crate::provider::ToolCall,
        kind: ToolKind,
    ) -> Option<crate::tools::policy::Approval> {
        self.parent.standing_approval(call, kind).await
    }

    /// A standing "always reject" is the user's own no; anything else
    /// this host refused was waiting on an approval nobody could give.
    fn declined_refusal(
        &self,
        approval: crate::tools::policy::Approval,
    ) -> crate::tools::policy::Refusal {
        use crate::tools::policy::{Approval, Refusal};
        match approval {
            Approval::RejectAlways => Refusal::UserDeclined,
            _ => Refusal::NeedsApproval,
        }
    }
}

/// Delegates a task to a specialised agent and returns only its final
/// answer. See the module docs for the three properties this exists to
/// establish.
pub struct SubagentTool {
    /// The definitions currently offered, swappable at run time (#265)
    /// so a config-tool write takes effect without a restart. Swappable
    /// *through* the lock, not by constructing a new tool: the
    /// `ToolSet` already holds this one behind an `Arc<dyn Tool>`, and
    /// a second `SubagentTool::new` would be a tool nothing dispatches
    /// to.
    ///
    /// `std::sync::RwLock` rather than `tokio::sync::RwLock`: every
    /// reader clones what it needs and drops the guard before any
    /// `.await`, so no guard is ever held across a suspension point and
    /// there is nothing an async lock would buy. It is also the lock
    /// that *works* here — [`Self::live_spec`] calls `build_spec`
    /// synchronously, with no runtime guaranteed to await on.
    agents: std::sync::RwLock<Vec<AgentDef>>,
    spec: ToolSpec,
    /// `(agent name, tool name)` pairs already warned about by
    /// [`Self::newly_unknown_tools`], so a typo in one definition's
    /// `tools:` list is logged once rather than once per delegation.
    warned_unknown_tools: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
    /// Whether the main turn's spec offers `background` (`[tools.background]
    /// max_per_session > 0`).
    background: bool,
    /// Handles currently running, for the duration of one call each, or
    /// of one background child's whole turn. Claimed and released
    /// through [`ResumeGuard`] — `Arc` so a background child's guard can
    /// move into its spawned task. A `Drop` guard, not a
    /// remember-to-remove-the-entry pattern, for
    /// the same reason `TerminalReservation`
    /// (`src/tools/acp_client.rs`) is one: two turns resuming the same
    /// handle concurrently would interleave writes into one history, and
    /// a resume that errors out — or whose turn is simply cancelled
    /// mid-flight, which ACP treats as routine — must still release the
    /// handle rather than leaving it refused forever.
    busy_handles: BusyHandles,
}

type BusyHandles = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

impl SubagentTool {
    /// `background` offers the `background` argument; pass
    /// `[tools.background] max_per_session > 0`.
    pub fn new(agents: Vec<AgentDef>, background: bool) -> Self {
        let spec = build_spec(&agents, background);
        Self {
            agents: std::sync::RwLock::new(agents),
            spec,
            warned_unknown_tools: std::sync::Mutex::new(std::collections::HashSet::new()),
            background,
            busy_handles: BusyHandles::default(),
        }
    }

    /// The definitions this tool currently offers.
    ///
    /// An owned clone, not a guard: every caller wants to `.await` or
    /// to build a string out of the list, and a guard held across an
    /// `.await` is precisely the coupling `ToolSet::execute`'s doc
    /// describes having removed. Cloning also means a concurrent
    /// [`Self::set_agents`] can never be observed half-applied — a
    /// caller sees the whole old list or the whole new one.
    fn agents(&self) -> Vec<AgentDef> {
        // Bound to its own name so the read guard's lifetime is this
        // statement and no longer: it is released before the clone is
        // even returned, let alone awaited on.
        let agents = self.agents.read().unwrap();
        agents.clone()
    }

    /// Swap the definition list — the write side of a hot reload, for
    /// the caller that has just rewritten a definition file (#265).
    ///
    /// `spec` is deliberately left alone: it stays the
    /// registration-time value it has always been (see
    /// [`Tool::spec`]). The list the model is *offered* is `ToolSet`'s
    /// copy of that spec, which the caller updates with
    /// `ToolSet::replace_spec` — pairing the two is what makes a written
    /// definition both offered and callable, rather than one of the two.
    ///
    /// Not `async`: it never waits, only writes. The lock is `std`'s,
    /// see the field's doc.
    pub fn set_agents(&self, agents: Vec<AgentDef>) {
        *self.agents.write().unwrap() = agents;
    }

    /// The spec for the definitions as they are *right now*, rather
    /// than [`Tool::spec`]'s construction-time one.
    ///
    /// `Tool::spec` hands back a borrow, so it cannot build this on
    /// demand from behind a lock — the guard would not outlive the
    /// call. This is that same `build_spec` made callable any time, for
    /// the caller that is about to store it (`ToolSet::replace_spec`)
    /// and wants the description the model is offered to list exactly
    /// the agents a dispatch would accept.
    pub fn live_spec(&self) -> ToolSpec {
        build_spec(&self.agents(), self.background)
    }

    /// Which of `def.tools`' names resolve to nothing the parent can
    /// currently see (`parent_visible`) — i.e. which of them
    /// `subagent_tool_specs` would silently drop — that have not been
    /// reported for this `def` before. Records them as reported and
    /// returns them, rather than logging directly, so the dedup logic
    /// is testable without capturing tracing output; the caller logs.
    ///
    /// `subagent` itself is excluded: `subagent_tool_specs` always
    /// removes it regardless of what a definition asks for, so naming
    /// it is not a typo, it is a no-op the model cannot exploit — see
    /// this module's doc.
    ///
    /// Nothing here removes the name or disables the definition. A
    /// typo in a `tools:` list is a mistake in one line, not a reason
    /// to silently take the rest of that agent's tools with it — still
    /// less the whole definition it belongs to.
    fn newly_unknown_tools(&self, def: &AgentDef, parent_visible: &[ToolSpec]) -> Vec<String> {
        let Some(allowed) = &def.tools else {
            return Vec::new();
        };
        let known: std::collections::HashSet<&str> =
            parent_visible.iter().map(|s| s.name.as_ref()).collect();
        let mut warned = self.warned_unknown_tools.lock().unwrap();
        let mut newly = Vec::new();
        for name in allowed {
            if name.as_str() == SUBAGENT_TOOL_NAME {
                continue;
            }
            if known.contains(name.as_str()) {
                continue;
            }
            if warned.insert((def.name.clone(), name.clone())) {
                newly.push(name.clone());
            }
        }
        newly
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    /// Synchronous only: a background child has to own the tool it runs
    /// on after this call returns, which `&self` cannot give it. The
    /// registered tool is the `Arc<SubagentTool>` below, which can.
    async fn execute(&self, input: &serde_json::Value) -> anyhow::Result<String> {
        let request = Request::parse(input)?;
        if request.background {
            anyhow::bail!(
                "background runs are not available here; call `subagent` \
                 without `background` to run it synchronously"
            );
        }
        self.run_sync(request.target, request.prompt).await
    }
}

/// One `subagent` call's arguments, parsed once for both `execute`s.
struct Request<'a> {
    prompt: &'a str,
    target: Target<'a>,
    background: bool,
}

/// A new child by definition name, or an existing one by handle.
enum Target<'a> {
    Dispatch(&'a str),
    Resume(&'a str),
}

impl<'a> Request<'a> {
    fn parse(input: &'a serde_json::Value) -> anyhow::Result<Self> {
        let prompt = input["prompt"].as_str().context("missing 'prompt'")?;
        let agent = string_field(input, "agent")?;
        let resume = string_field(input, "resume")?;
        let background = match input.get("background") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => anyhow::bail!("'background' must be a boolean"),
        };

        // Neither given, or both given, is a recoverable error naming
        // the rule rather than guessing which one was meant.
        let target = match (agent, resume) {
            (Some(_), Some(_)) => anyhow::bail!(
                "'agent' and 'resume' are mutually exclusive: dispatch a new \
                 agent by name, or continue an existing one by its handle — \
                 not both in the same call"
            ),
            (None, None) => anyhow::bail!(
                "either 'agent' (to dispatch a new subagent) or 'resume' \
                 (to continue one by handle) is required"
            ),
            (Some(name), None) => Target::Dispatch(name),
            (None, Some(handle)) => Target::Resume(handle),
        };
        Ok(Self {
            prompt,
            target,
            background,
        })
    }
}

/// Lets one `Arc<SubagentTool>` back both the `subagent` slot in `ToolSet`
/// *and* the `Weak` `ConfigTool` holds in its `agent_config`
/// instance — the same shape `SkillTool` uses for its four slots. Without
/// it the tool would have to be owned by the set alone, and a definition
/// written at run time would have nowhere to be swapped into (#265).
#[async_trait]
impl Tool for Arc<SubagentTool> {
    fn kind(&self) -> ToolKind {
        (**self).kind()
    }

    fn spec(&self) -> &ToolSpec {
        (**self).spec()
    }

    async fn execute(&self, input: &serde_json::Value) -> anyhow::Result<String> {
        self.execute_arc(input).await
    }
}

impl SubagentTool {
    /// The `execute` the registered tool runs: synchronous, or with
    /// `background`, started in a task of its own.
    async fn execute_arc(self: &Arc<Self>, input: &serde_json::Value) -> anyhow::Result<String> {
        let request = Request::parse(input)?;
        if request.background {
            return self.start_background(request.target, request.prompt);
        }
        self.run_sync(request.target, request.prompt).await
    }

    /// Run the child to completion inside this call, under the parent's
    /// own host, and return its answer.
    async fn run_sync(&self, target: Target<'_>, prompt: &str) -> anyhow::Result<String> {
        let (ctx, prepared) = self.prepare(target, prompt)?;
        let Prepared {
            def,
            handle,
            created_at,
            mut history,
            over_cap_reason,
            guard: _guard,
        } = prepared;
        // The parent's host, deliberately: a permission request from a
        // subagent must reach the same person, judged by the same
        // origin. A different host here would make delegation a way
        // around the gate. `turn_error` is the one method NOT forwarded
        // unchanged — see `SubagentHost`'s doc for why a
        // subagent's own provider failure must not report itself as
        // *this request's* terminal outcome.
        let progress: Arc<dyn crate::serve::TurnHost> =
            Arc::new(SubagentHost(Arc::clone(&ctx.progress)));
        // The subagent's own memory-tool calls (if it has any) write
        // under the same namespace the delegating conversation is in —
        // a fact about where this deployment's memory lives, not a
        // personality trait, so it travels through like the date does
        // rather than being stripped like the workspace files are.
        let namespace = crate::tools::workspace_tools::current_memory_namespace();
        self.run_and_store(
            &ctx,
            &def,
            &handle,
            created_at,
            &mut history,
            over_cap_reason,
            progress,
            namespace,
        )
        .await
    }

    /// Start the child in a spawned task and return at once; its outcome
    /// is registered in `ServeState::background`, which delivers it into
    /// this session's next turn (or between this turn's rounds).
    ///
    /// Only a main turn may: it needs a session to deliver into, and a
    /// subagent's own turn has none. The child runs under
    /// [`BackgroundSubagentHost`], so it never asks for permission, and
    /// it holds its handle's [`ResumeGuard`] until it ends, so a resume
    /// of the same handle meanwhile is refused rather than interleaved.
    ///
    /// Task-locals do not cross `tokio::spawn`, so the spawned future
    /// re-scopes those the tools read, in `TurnLoop::run`'s order.
    fn start_background(
        self: &Arc<Self>,
        target: Target<'_>,
        prompt: &str,
    ) -> anyhow::Result<String> {
        let ctx = crate::serve::current_turn_context().context("no turn to delegate from")?;
        if !self.background {
            anyhow::bail!(
                "background runs are turned off on this deployment \
                 ([tools.background] max_per_session = 0); call `subagent` \
                 without `background` to run it synchronously"
            );
        }
        let session = match (&ctx.session_id, ctx.subagent_depth) {
            (Some(session), 0) => session.clone(),
            _ => anyhow::bail!(
                "background subagents can only be started from the main \
                 conversation, not from inside a subagent or a turn without \
                 a session; call `subagent` without `background` to run it \
                 synchronously"
            ),
        };
        let fresh = matches!(target, Target::Dispatch(_));
        let (ctx, prepared) = self.prepare(target, prompt)?;
        let Prepared {
            def,
            handle,
            created_at,
            mut history,
            over_cap_reason,
            guard,
        } = prepared;

        let kind = crate::background::TaskKind::Subagent {
            agent: def.name.clone(),
            handle: handle.clone(),
        };
        // Past the cap this refuses, and `guard` drops on the way out.
        let id = ctx.state.background.register(
            &session,
            kind.clone(),
            background_label(&def.name, prompt),
        )?;
        // A fresh handle has nothing stored until the child finishes, so
        // one cancelled or cut off by a restart could not be resumed.
        // Storing its prompt now makes it resumable from the start. A
        // resume already has its stored history.
        if fresh {
            persist(
                ctx.state.subagent_cache.as_deref(),
                &handle,
                &def.name,
                history.clone(),
                created_at,
                over_cap_reason,
            );
        }
        let started = format!(
            "Started background task {id}: subagent \"{}\" (handle {handle}). \
             Its result will be delivered to this conversation when it finishes.",
            def.name
        );

        // The child outlives the request that started it, so it must not
        // keep that request's host (an HTTP turn's `SseProgress` holds the
        // response stream open) or the context carrying it. It gets the
        // host's detached form and a context of its own around that.
        let detached = ctx.progress.detached();
        let task_ctx = Arc::new(crate::serve::TurnContext {
            state: Arc::clone(&ctx.state),
            provider: Arc::clone(&ctx.provider),
            progress: Arc::clone(&detached),
            visible_specs: Arc::clone(&ctx.visible_specs),
            timer_origin: ctx.timer_origin.clone(),
            session_id: ctx.session_id.clone(),
            subagent_depth: ctx.subagent_depth,
            admin_room_profile: ctx.admin_room_profile.clone(),
        });
        let refused = Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress: Arc<dyn crate::serve::TurnHost> = Arc::new(BackgroundSubagentHost::new(
            detached,
            Arc::clone(&refused),
            handle.clone(),
        ));
        let namespace = crate::tools::workspace_tools::current_memory_namespace();
        let tool = Arc::clone(self);
        let scope_ctx = Arc::clone(&task_ctx);
        let task_session = session.clone();
        let task_id = id.clone();
        let task_namespace = namespace.clone();
        let run = async move {
            // A panic in a tool or the provider must still complete the
            // task, or it would hold a cap slot and keep the session
            // waiting on it until a restart. Caught inside the spawned
            // task, so the attached abort handle still targets it.
            let result = std::panic::AssertUnwindSafe(tool.run_and_store(
                &task_ctx,
                &def,
                &handle,
                created_at,
                &mut history,
                over_cap_reason,
                progress,
                task_namespace,
            ))
            .catch_unwind()
            .await;
            // Free the handle before the completion lands, so a resume
            // that the completion prompts is not refused as busy.
            drop(guard);
            let outcome = match result {
                Ok(Ok(text)) => crate::background::Outcome::Finished {
                    text,
                    refused: std::mem::take(
                        &mut *refused.lock().unwrap_or_else(|e| e.into_inner()),
                    ),
                },
                Ok(Err(e)) => crate::background::Outcome::Failed {
                    message: e.to_string(),
                },
                Err(payload) => crate::background::Outcome::Failed {
                    message: panic_message(payload.as_ref()),
                },
            };
            // `false` when the task was cancelled meanwhile: the
            // `Cancelled` completion already stands, and this one is
            // dropped.
            if task_ctx
                .state
                .background
                .complete(&task_session, &task_id, outcome.clone())
            {
                let completion = crate::background::Completion {
                    task_id,
                    kind,
                    outcome,
                    finished_at: chrono::Utc::now(),
                };
                let resumable = crate::background::resumable(
                    task_ctx.state.subagent_cache.as_deref(),
                    &completion,
                );
                let report = crate::background::render(&completion, resumable);
                task_ctx.progress.background_finished(&report).await;
            }
        };
        let task = tokio::spawn(scope_background(scope_ctx, namespace, run));
        ctx.state
            .background
            .attach(&session, &id, task.abort_handle());
        Ok(started)
    }

    /// Everything a child run needs before its turn starts, resolved
    /// from a dispatch or a resume — with the handle already claimed.
    ///
    /// Each branch looks things up in the order its errors should win,
    /// which is why the turn context is fetched inside rather than
    /// passed in: a dispatch names an unknown agent before it complains
    /// that there is no turn, a resume the other way round.
    fn prepare(
        &self,
        target: Target<'_>,
        prompt: &str,
    ) -> anyhow::Result<(Arc<crate::serve::TurnContext>, Prepared)> {
        match target {
            Target::Dispatch(name) => self.prepare_dispatch(name, prompt),
            Target::Resume(handle) => self.prepare_resume(handle, prompt),
        }
    }

    /// A fresh child conversation with `def`, under a freshly generated
    /// handle.
    fn prepare_dispatch(
        &self,
        name: &str,
        prompt: &str,
    ) -> anyhow::Result<(Arc<crate::serve::TurnContext>, Prepared)> {
        let agents = self.agents();
        let Some(def) = agents.iter().find(|a| a.name == name) else {
            // Recoverable: the parent picked a name that does not
            // exist, and can pick again if it is told what does.
            let known: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
            anyhow::bail!("no agent named '{name}'. Available: {}", known.join(", "));
        };

        // Only a live turn has a model, a permission host and a visible
        // tool set to lend. Nothing else can delegate.
        let ctx = crate::serve::current_turn_context().context("no turn to delegate from")?;

        let history = vec![ChatMessage::user(prompt)];
        // `Uuid::now_v7`'s hyphenated `Display` form is ASCII hex plus
        // `-`, which `SubagentCache::path_for`'s filename guard accepts
        // outright — no reserved-name collision is possible, and
        // nothing here needs the shorter `simple` rendering.
        let handle = uuid::Uuid::now_v7().to_string();
        let created_at = chrono::Utc::now();

        // Nobody else can know a handle this fresh, so the claim always
        // succeeds; it is taken so a background child holds its handle
        // the same way a resumed one does.
        let guard = ResumeGuard::claim(&self.busy_handles, &handle)?;

        // Nothing is on disk yet under this fresh handle, so an
        // over-cap `put` here just means the handle never becomes
        // resumable at all — there is no earlier, shorter copy for the
        // model to be misled by (contrast `resume`'s own message).
        Ok((
            ctx,
            Prepared {
                def: def.clone(),
                handle,
                created_at,
                history,
                over_cap_reason: "history exceeded the cache limit",
                guard,
            },
        ))
    }

    /// Continue the child conversation stored under `handle`.
    ///
    /// The definition is reloaded by its stored *name* from `self.agents`
    /// — the current, live list — never from anything stored alongside
    /// the history. That is what makes `subagent_tool_specs` recompute
    /// the offered tool list on resume instead of restoring a stale one:
    /// see the module doc's third property. A stored tool list would
    /// reopen the hole `TurnLoop::run`'s offer gate exists to close, by
    /// letting a resumed child carry forward a list wider than its
    /// current definition allows (or than the current parent turn can
    /// even see).
    fn prepare_resume(
        &self,
        handle: &str,
        prompt: &str,
    ) -> anyhow::Result<(Arc<crate::serve::TurnContext>, Prepared)> {
        let ctx = crate::serve::current_turn_context().context("no turn to delegate from")?;

        // Claimed before the child's turn can `.await`, so a concurrent
        // resume of the same handle always observes it already held —
        // see `ResumeGuard`'s doc. Released on every exit path via
        // `Drop`, success, error, or the whole call (or background task)
        // being cancelled mid-flight.
        let guard = ResumeGuard::claim(&self.busy_handles, handle)?;

        // Worded differently from the "no subagent is stored under this
        // handle" miss just below on purpose: that one means a real
        // cache was checked and came up empty for this handle. This one
        // means there is no cache at all on this deployment, so *no*
        // handle could ever resolve — reporting it identically would
        // read as "this particular handle is unknown," which invites
        // retrying with a different one, when the real fix is dispatch.
        // `persist`'s `Resumability::NotResumable("no resume cache is
        // configured on this deployment")` already words its equivalent
        // this way; this matches it.
        let Some(cache) = &ctx.state.subagent_cache else {
            anyhow::bail!(
                "resume is not available: no resume cache is configured on \
                 this deployment. Dispatch a new subagent instead, with \
                 `agent` set."
            );
        };
        let Some(stored) = cache.get(handle) else {
            anyhow::bail!(
                "no subagent is stored under handle '{handle}' — dispatch a \
                 new one instead, with `agent` set"
            );
        };

        let agents = self.agents();
        let Some(def) = agents.iter().find(|a| a.name == stored.agent) else {
            // The definition this handle belongs to does not currently
            // resolve — a rename, a deletion, or simply a `.md` that
            // failed to parse this time around. That last case is not
            // permanent: `agents::load_agents_dir` skips a single file
            // it can't read or parse (a `warn!`, nothing more) rather
            // than failing the whole load, so a definition can vanish
            // on one restart — mid-save, a YAML typo — and come back on
            // the next once it's fixed, with this same handle still
            // valid. Deleting the stored child here on what might be a
            // transient miss would make that restart unrecoverable
            // instead of merely inconvenient, so this only bails; the
            // entry is left for `prune_before` to retire on its own
            // schedule if the definition truly never comes back.
            anyhow::bail!(
                "the '{}' agent definition that handle '{handle}' belongs \
                 to does not currently exist; dispatch a new agent instead, \
                 or try resuming again once it's back",
                stored.agent
            );
        };

        let mut history = stored.history;
        history.push(ChatMessage::user(prompt));

        // Unlike `dispatch`, this handle already resolves to a shorter,
        // previously-`put` history on disk — an over-cap `put` here
        // writes nothing, leaving that earlier copy in place. Left
        // unremarked, a model reading only "not resumable: history
        // exceeded the cache limit" would have no way to know the
        // handle still resolves, just not to what it was told: a later
        // resume would silently rewind past this exact exchange, losing
        // this prompt and its answer with no signal that anything
        // diverged. Name that explicitly rather than reusing dispatch's
        // message.
        Ok((
            ctx,
            Prepared {
                def: def.clone(),
                handle: handle.to_string(),
                created_at: stored.created_at,
                history,
                over_cap_reason: "this exchange exceeded the cache limit and was not saved; \
                     the stored copy still ends before it, so a later resume \
                     will not include this prompt or its answer",
                guard,
            },
        ))
    }

    /// Run `def`'s nested `TurnLoop` to completion on `history`, then
    /// store the result under `handle` and prefix the answer with what
    /// that store attempt means for resumability. Shared by
    /// [`Self::dispatch`] (a freshly generated handle) and
    /// [`Self::resume`] (the same handle it was given), so this
    /// run-then-persist sequence is written once — `over_cap_reason` is
    /// the one thing the two callers have to say differently, since
    /// only `resume` risks leaving a stale, shorter copy behind when
    /// `put` refuses.
    ///
    /// The `Err`s are a turn that outran `[tools.subagent]
    /// turn_timeout_secs` — see [`timed_out`] — and a provider failure —
    /// see [`provider_error`].
    ///
    /// `progress` is the host the nested turn runs under and `namespace`
    /// its memory namespace: the caller's, read inside the parent's
    /// round for a synchronous run, captured before the spawn for a
    /// background one.
    #[allow(clippy::too_many_arguments)]
    async fn run_and_store(
        &self,
        ctx: &crate::serve::TurnContext,
        def: &AgentDef,
        handle: &str,
        created_at: chrono::DateTime<chrono::Utc>,
        history: &mut Vec<ChatMessage>,
        over_cap_reason: &'static str,
        progress: Arc<dyn crate::serve::TurnHost>,
        namespace: String,
    ) -> anyhow::Result<String> {
        // A typo, a renamed tool, or a name that was never registered
        // yields a subagent silently missing it — no warning at load,
        // no error at call time, it is simply never offered. Warn
        // rather than fail: one bad name in the list must not take the
        // rest of it, or the definition, down.
        for name in self.newly_unknown_tools(def, &ctx.visible_specs) {
            warn!(
                "agent '{}': tools list names '{name}', which this delegation \
                 cannot currently see — it is not a registered tool, or not \
                 one visible on this transport right now. The definition \
                 still runs; that name is simply never offered to it.",
                def.name
            );
        }

        let system = subagent_system_prompt(def);
        let mut specs = subagent_tool_specs(
            def,
            &ctx.visible_specs,
            &self.agents(),
            ctx.subagent_depth,
            ctx.state.config.tools.subagent.max_depth,
        );
        // `ask_human` needs a host that can ask: a background child's
        // forwards the session's identity, a synchronous child's has none,
        // and a tool that can only refuse is worse than absent.
        if progress.consult_identity().is_none() {
            specs.retain(|s| s.name.as_ref() != crate::tools::ask_human::NAME);
        }

        // A definition that pins a profile runs on that profile's provider —
        // resolved through the very same `ProviderRegistry::for_profile` that
        // room/session turns use, so profile-resolution semantics exist in one
        // function, not two. The "unknown name" policy that future
        // client/local-loop configs may want to change (warn + fallback
        // instead of failing startup) is a change to that one function; a
        // server config cannot reach it at runtime —
        // `Config::validate_subagent_profiles` bails at startup first.
        // `fallback_provider` wrapping happens inside `for_profile`, so a
        // profiled subagent inherits the refusal-fallback behaviour for free.
        let provider: std::sync::Arc<dyn crate::provider::Provider> = match def.profile.as_deref() {
            Some(name) => ctx.state.registry.for_profile(&ctx.state.config, name),
            None => std::sync::Arc::clone(&ctx.provider),
        };

        let turn = crate::serve::TurnLoop {
            state: &ctx.state,
            provider: &provider,
            system: Some(&system),
            tool_specs: &specs,
            progress: &progress,
            timer_origin: ctx.timer_origin.clone(),
            admin_room_profile: ctx.admin_room_profile.clone(),
            namespace,
            // This nested turn sits one level below whoever delegated to
            // it, so it can delegate further exactly when
            // `subagent_depth + 1` is still under the cap.
            // `subagent_tool_specs` reads the same `ctx.subagent_depth`
            // this is derived from to decide whether to offer `subagent`
            // at all.
            subagent_depth: ctx.subagent_depth + 1,
            // No session behind it. The conversation exists for the
            // length of this call and is then dropped — that is what
            // "context isolation" means here. Resumability is a
            // separate, workspace-external mechanism (`SubagentCache`),
            // not a promise that this history reaches the store.
            persistence: None,
        }
        .run(history);
        // Delegation must not inherit its parent's conversation attachments,
        // even when the entire child is polled in the parent's task.
        let turn = crate::tools::attachment_tools::scope_attachment_owner(None, turn);

        // The whole turn runs under one deadline, on top of the provider's
        // own idle deadline — see `SubagentConfig` for what this catches
        // that that one cannot.
        let (text, stop) = match ctx.state.config.tools.subagent.turn_timeout() {
            None => turn.await,
            Some(limit) => match tokio::time::timeout(limit, turn).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    return Err(timed_out(
                        ctx,
                        def,
                        handle,
                        created_at,
                        history,
                        over_cap_reason,
                        limit,
                    ));
                }
            },
        };

        // A provider failure (rate limit, insufficient credit, a network
        // error) is not an answer to prefix and return `Ok` with — see
        // `provider_error`'s doc for what that used to look like from the
        // parent's side.
        if let crate::serve::TurnStop::ProviderError { message } = &stop {
            return Err(provider_error(
                ctx,
                def,
                handle,
                created_at,
                history,
                over_cap_reason,
                message,
            ));
        }

        let answer = answer_text(text, stop);
        let history = std::mem::take(history);
        let resumability = persist(
            ctx.state.subagent_cache.as_deref(),
            handle,
            &def.name,
            history,
            created_at,
            over_cap_reason,
        );
        Ok(prefixed(&def.name, resumability, &answer))
    }
}

/// What [`SubagentTool::prepare`] resolves: the definition, the handle
/// (claimed through `guard`), and the history the child's turn starts
/// from.
struct Prepared {
    def: AgentDef,
    handle: String,
    created_at: chrono::DateTime<chrono::Utc>,
    history: Vec<ChatMessage>,
    over_cap_reason: &'static str,
    guard: ResumeGuard,
}

/// The one-line description a background task is listed under: the
/// agent and the start of its prompt.
fn background_label(agent: &str, prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or("");
    let short: String = first_line.chars().take(80).collect();
    let more = if short.len() < prompt.trim_end().len() {
        "\u{2026}"
    } else {
        ""
    };
    format!("subagent \"{agent}\": {short}{more}")
}

/// What a background child's caught panic is reported as: the payload
/// when it is the usual `&str` or `String`, else just the fact.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match detail {
        Some(d) => format!("subagent panicked: {d}"),
        None => "subagent panicked".to_string(),
    }
}

/// `fut` with the task-locals a tool call reads, re-scoped for a spawned
/// background child: they do not cross `tokio::spawn`. The same scopes,
/// in the same order, as `TurnLoop::run`'s tool execution. The avatar
/// scope is left out: [`BackgroundSubagentHost`] has no avatar, and an
/// unscoped avatar reads as none.
async fn scope_background<F: std::future::Future>(
    ctx: Arc<crate::serve::TurnContext>,
    namespace: String,
    fut: F,
) -> F::Output {
    let fut = crate::tools::workspace_tools::scope_memory_namespace(namespace, fut);
    let fut =
        crate::tools::config_tools::scope_admin_room_profile(ctx.admin_room_profile.clone(), fut);
    let origin = ctx.timer_origin.clone();
    let client = ctx.progress.acp_client();
    let fut = crate::serve::scope_turn_context(ctx, fut);
    match (origin, client) {
        (Some(o), Some(c)) => {
            crate::timer::scope_timer_origin(o, crate::tools::acp_client::scope_acp_client(c, fut))
                .await
        }
        (Some(o), None) => crate::timer::scope_timer_origin(o, fut).await,
        (None, Some(c)) => crate::tools::acp_client::scope_acp_client(c, fut).await,
        (None, None) => fut.await,
    }
}

/// Wind up a subagent turn that outran its deadline: save what it had, and
/// build the error the parent model reads in place of an answer.
///
/// An error rather than an answer with a marker, because nothing was
/// answered — the parent has to decide what to do next (resume, retry
/// smaller, or carry on without it), and a tool error is the shape that
/// says so. Before this existed the same situation said nothing at all:
/// the `subagent` call just never returned (#258).
///
/// `history` is repaired before it is stored. The turn was dropped
/// wherever it happened to be, and that can be between pushing an
/// assistant message's `tool_use` and pushing the `tool_result` answering
/// it — a history the provider API rejects, which would make the saved
/// handle fail on its first resume instead of continuing.
/// `repair_tool_pairing` answers such a call with the same placeholder a
/// cache miss produces, which tells the model to call the tool again if it
/// still needs it; that is exactly right for a call that never finished.
fn timed_out(
    ctx: &crate::serve::TurnContext,
    def: &AgentDef,
    handle: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    history: &mut Vec<ChatMessage>,
    over_cap_reason: &'static str,
    limit: std::time::Duration,
) -> anyhow::Error {
    let secs = limit.as_secs();
    warn!(
        "subagent '{}' (handle {handle}) did not finish its turn within {secs}s; \
         stopping it and reporting a timeout to the parent",
        def.name
    );
    let history = crate::session_storage::repair_tool_pairing(std::mem::take(history));
    let resumability = persist(
        ctx.state.subagent_cache.as_deref(),
        handle,
        &def.name,
        history,
        created_at,
        over_cap_reason,
    );
    let saved = match resumability {
        Resumability::Resumable(handle) => format!(
            "Its conversation so far was saved: pass handle {handle} as `resume` \
             to continue it."
        ),
        Resumability::NotResumable(reason) => {
            format!("Its conversation so far is not resumable: {reason}.")
        }
    };
    anyhow::anyhow!(
        "subagent '{}' timed out: its turn did not finish within {secs}s \
         ([tools.subagent] turn_timeout_secs) and was stopped. {saved}",
        def.name
    )
}

/// Wind up a subagent turn that ended because its own `Provider::chat`
/// call errored (a rate limit, insufficient API credit, a network
/// failure, ...): save what it had, and build the error the parent model
/// reads in place of an answer.
///
/// Before this existed, `TurnStop::ProviderError` carried no message and
/// `answer_text`'s fallback arm turned every such failure into the same
/// generic `"[the subagent produced no answer]"` text — handed back as a
/// *successful* tool result, indistinguishable from a subagent that simply
/// gave up. The actual cause reached only the `error!("Provider error:
/// {e:#}")` log line in `run_llm_turn` (`src/serve/mod.rs`), never the
/// parent model, which would go on to quietly redo the delegated work
/// itself with no idea the subagent's provider — not the task — was what
/// failed. `TurnStop::ProviderError { message }` is what closes that: the
/// same text logged there now travels with the outcome, and this function
/// is what turns it into the tool's own `Err` rather than folding it into
/// `answer_text`'s generic case.
///
/// Unlike [`timed_out`], `history` needs no `repair_tool_pairing`: the
/// failing `provider.chat` call is the first thing each round does, ahead
/// of any history mutation for that round (`TurnLoop::run`,
/// `src/serve/mod.rs`), so whatever is here is exactly what the last
/// *successful* round left — already correctly paired, never mid-`tool_use`.
fn provider_error(
    ctx: &crate::serve::TurnContext,
    def: &AgentDef,
    handle: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    history: &mut Vec<ChatMessage>,
    over_cap_reason: &'static str,
    message: &str,
) -> anyhow::Error {
    warn!(
        "subagent '{}' (handle {handle})'s provider call failed: {message}; \
         reporting the error to the parent instead of a placeholder answer",
        def.name
    );
    let history = std::mem::take(history);
    let resumability = persist(
        ctx.state.subagent_cache.as_deref(),
        handle,
        &def.name,
        history,
        created_at,
        over_cap_reason,
    );
    let saved = match resumability {
        Resumability::Resumable(handle) => format!(
            "Its conversation so far was saved: pass handle {handle} as `resume` \
             to continue it, once the underlying problem is resolved."
        ),
        Resumability::NotResumable(reason) => {
            format!("Its conversation so far is not resumable: {reason}.")
        }
    };
    anyhow::anyhow!("subagent '{}' failed: {message}. {saved}", def.name)
}

/// How a dispatched or resumed child's answer should describe its own
/// resumability to the model — see [`prefixed`].
enum Resumability {
    Resumable(String),
    NotResumable(&'static str),
}

/// Persist `history` under `handle` if a cache is configured, choosing
/// how the caller should describe resumability.
///
/// Never truncates: `SubagentCache::put` refuses an over-cap entry
/// wholesale rather than dropping old messages to fit (see its own doc
/// for why — a `tool_use` cut loose from its matching `tool_result`
/// produces a history the provider API rejects outright, making the
/// entry unloadable rather than merely shorter). So `Ok(false)` here
/// still returns the answer normally; only the resumability marker
/// changes.
///
/// `over_cap_reason` is the caller's choice of wording for that
/// `Ok(false)` case specifically — `dispatch` and `resume` need
/// different ones, since only `resume` risks leaving a stale, shorter
/// copy on disk when `put` refuses to overwrite it (see `resume`'s own
/// comment at its call site).
fn persist(
    cache: Option<&crate::subagent_cache::SubagentCache>,
    handle: &str,
    agent_name: &str,
    history: Vec<ChatMessage>,
    created_at: chrono::DateTime<chrono::Utc>,
    over_cap_reason: &'static str,
) -> Resumability {
    let Some(cache) = cache else {
        return Resumability::NotResumable("no resume cache is configured on this deployment");
    };
    let child = crate::subagent_cache::StoredChild {
        agent: agent_name.to_string(),
        history,
        created_at,
        updated_at: chrono::Utc::now(),
    };
    match cache.put(handle, &child) {
        Ok(true) => Resumability::Resumable(handle.to_string()),
        Ok(false) => Resumability::NotResumable(over_cap_reason),
        Err(e) => {
            warn!("subagent cache: failed to store handle '{handle}': {e:#}");
            Resumability::NotResumable("could not be saved to the resume cache")
        }
    }
}

/// Prefix a child's answer with its handle, or with why it has none.
fn prefixed(agent_name: &str, resumability: Resumability, answer: &str) -> String {
    match resumability {
        Resumability::Resumable(handle) => {
            format!("[subagent {agent_name} · handle {handle}]\n{answer}")
        }
        Resumability::NotResumable(reason) => {
            format!("[subagent {agent_name} · not resumable: {reason}]\n{answer}")
        }
    }
}

/// What the parent model is told for a nested turn's own outcome.
///
/// `TurnStop::ProviderError` never reaches here: `run_and_store` matches it
/// out beforehand and returns [`provider_error`]'s `Err` instead, since a
/// failed provider call is not an answer to fold into a successful tool
/// result. Only `Replied` (`text` always `Some`) and `BudgetExhausted`
/// reach this function in practice; the fallback arm stays as a defensive
/// default for a future `TurnStop` variant this function was not updated
/// for, rather than an `unreachable!` that would turn that omission into a
/// panic.
fn answer_text(text: Option<String>, stop: crate::serve::TurnStop) -> String {
    match stop {
        crate::serve::TurnStop::BudgetExhausted { partial_text } => {
            format!("[the subagent used its whole tool budget without finishing]\n\n{partial_text}")
        }
        _ => text.unwrap_or_else(|| "[the subagent produced no answer]".to_string()),
    }
}

/// Holds one run's exclusive claim on a handle: for the duration of the
/// call, or of a background child's whole turn. It owns its `Arc` to
/// the busy set, so a background child's guard moves into its spawned
/// task and is released when that task ends or is aborted.
///
/// A `Drop` guard rather than a remember-to-remove-the-entry pattern,
/// for the same reason `TerminalReservation` (`src/tools/acp_client.rs`)
/// is one: a resume that returns an error, or whose whole call is
/// cancelled mid-flight (the turn's future simply dropped — ACP treats
/// that as routine, no different from an Escape in the editor or a
/// dropped socket), must still release the handle, or it stays refused
/// forever. `claim` does all its work synchronously — a `Mutex` lock,
/// an insert — before this method's first `.await`, so two concurrent
/// resumes of the same handle can never both observe it free.
struct ResumeGuard {
    busy: BusyHandles,
    handle: String,
}

impl ResumeGuard {
    /// Claim `handle`, refusing if another run already holds it.
    fn claim(busy: &BusyHandles, handle: &str) -> anyhow::Result<Self> {
        let mut set = busy.lock().unwrap();
        if !set.insert(handle.to_string()) {
            anyhow::bail!(
                "subagent handle '{handle}' is already in use by another \
                 resume or a background run — wait for it to finish before \
                 resuming again"
            );
        }
        Ok(Self {
            busy: Arc::clone(busy),
            handle: handle.to_string(),
        })
    }
}

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        self.busy.lock().unwrap().remove(&self.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> Vec<crate::agents::AgentDef> {
        vec![crate::agents::AgentDef {
            name: "reviewer".to_string(),
            description: "Reviews a diff.".to_string(),
            tools: Some(vec!["file_read".to_string()]),
            subagents: None,
            prompt: "You are a reviewer.".to_string(),
            profile: None,
        }]
    }

    /// The definitions moved behind a lock in #265, so a test that wants
    /// one out of the tool clones it — the same thing every reader in
    /// this module does. The list is still `new`'s, unchanged, so this
    /// is the registration-time definition; only the access changed.
    fn agent(tool: &SubagentTool, index: usize) -> crate::agents::AgentDef {
        tool.agents()[index].clone()
    }

    /// The description is the parent model's only basis for choosing,
    /// so every agent's own description has to reach it.
    #[test]
    fn the_tool_description_lists_every_agent() {
        let spec = SubagentTool::new(defs(), false).spec().clone();
        assert!(
            spec.description.contains("reviewer"),
            "{}",
            spec.description
        );
        assert!(
            spec.description.contains("Reviews a diff."),
            "{}",
            spec.description
        );
    }

    #[test]
    fn the_kind_is_other() {
        assert_eq!(SubagentTool::new(defs(), false).kind(), ToolKind::Other);
    }

    /// A definition written after the tool was constructed is callable
    /// and offered without a restart — the whole point of holding the
    /// list behind a lock.
    ///
    /// `spec()` deliberately stays put: it is the registration-time
    /// value, and `Tool::spec` cannot return a borrow of anything
    /// rebuilt on demand. What the model is offered is `ToolSet`'s copy
    /// of the spec, updated via `replace_spec` — so `live_spec` is the
    /// one that has to track `set_agents`.
    #[tokio::test]
    async fn set_agents_makes_a_new_definition_callable() {
        let tool = SubagentTool::new(Vec::new(), false);
        assert!(tool.live_spec().description.contains("none yet"));
        tool.set_agents(vec![crate::agents::AgentDef {
            name: "reviewer".into(),
            description: "Reviews things.".into(),
            tools: None,
            subagents: None,
            prompt: "Review.".into(),
            profile: None,
        }]);
        assert!(
            tool.live_spec()
                .description
                .contains("- reviewer: Reviews things.")
        );
        // The tool's own `spec()` is the registration-time value and stays put.
        assert!(!tool.spec().description.contains("reviewer"));
    }

    /// A name the operator never defined is a mistake the parent can
    /// recover from — list what exists rather than just refusing.
    #[tokio::test]
    async fn an_unknown_agent_names_the_ones_that_exist() {
        let err = SubagentTool::new(defs(), false)
            .execute(&serde_json::json!({"agent": "nope", "prompt": "x"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("reviewer"), "{err}");
    }

    /// Outside a turn there is nothing to delegate with. Refusing here
    /// is what keeps the tool honest on any path that is not a live
    /// turn.
    #[tokio::test]
    async fn delegating_outside_a_turn_refuses() {
        let err = SubagentTool::new(defs(), false)
            .execute(&serde_json::json!({"agent": "reviewer", "prompt": "x"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no turn"), "{err}");
    }

    /// The point of the feature: a subagent's system prompt is its own
    /// definition and nothing else. A `SOUL.md` in the workspace must
    /// not reach it — and neither does a clock, which would change the
    /// prompt on every call and cost a resumed subagent its prompt
    /// cache. `current_time` is how it asks.
    #[test]
    fn the_system_prompt_is_the_definition_body_and_nothing_else() {
        let sys = subagent_system_prompt(&defs()[0]);
        assert_eq!(sys, defs()[0].prompt);
        assert!(sys.contains("You are a reviewer."));
        assert!(
            !sys.contains("Current Date and Time"),
            "a clock in the prompt is what `current_time` replaced: {sys}"
        );
        for absent in ["# Soul", "# Identity", "# User", "# Agent Instructions"] {
            assert!(
                !sys.contains(absent),
                "{absent} must not be inherited: {sys}"
            );
        }
    }

    /// The depth cap bites where it was always going to bite: the list
    /// a turn is actually *offered*. A turn at or past
    /// `tools.subagent.max_depth` gets no `subagent` spec in its own
    /// list at all — and the list is what the gate enforces (see the
    /// module doc's third property and
    /// `a_subagent_cannot_nest_past_max_depth_one` in `src/serve/mod.rs`),
    /// so a call naming it anyway is refused as not offered. Below the
    /// cap, an unrestricted definition inherits the delegator's own
    /// `subagent` spec verbatim.
    #[test]
    fn the_subagent_tool_is_offered_only_below_the_depth_cap() {
        let parent_visible = [spec_named("file_read"), spec_named(SUBAGENT_TOOL_NAME)];
        let all = defs();

        // At the cap: under `max_depth = 1` only the main turn (depth 0)
        // delegates — a list built for the depth-1 turn it produced, and
        // anything deeper, carries no `subagent` — restricted definition
        // and unrestricted definition both.
        let inherited = subagent_tool_specs(&all[0], &parent_visible, &all, 1, 1);
        assert!(!inherited.iter().any(|s| s.name == SUBAGENT_TOOL_NAME));

        let unrestricted = crate::agents::AgentDef {
            tools: None,
            ..all[0].clone()
        };
        let inherited = subagent_tool_specs(&unrestricted, &parent_visible, &all, 1, 1);
        assert!(!inherited.iter().any(|s| s.name == SUBAGENT_TOOL_NAME));
        assert!(inherited.iter().any(|s| s.name == "file_read"));

        // The shipped default (`max_depth = 2`): the list built at
        // depth 1 — for the turn a depth-1 agent would produce — carries
        // no `subagent`: main(0) may delegate to plan(1), and plan(1)
        // itself still holds the tool, but what plan(1) produces sits at
        // the cap and cannot delegate further.
        let inherited = subagent_tool_specs(&unrestricted, &parent_visible, &all, 1, 2);
        assert!(!inherited.iter().any(|s| s.name == SUBAGENT_TOOL_NAME));

        // Below the cap — a main turn (depth 0, default cap 2) — an
        // unrestricted definition inherits the delegator's own spec
        // verbatim: same description, same list it carries.
        let inherited = subagent_tool_specs(&unrestricted, &parent_visible, &all, 0, 2);
        let offered = inherited
            .iter()
            .find(|s| s.name == SUBAGENT_TOOL_NAME)
            .expect("a delegator below the cap is offered `subagent`");
        assert_eq!(offered.description, parent_visible[1].description);
    }

    /// `subagents:` narrows, it does not grant. The spec offered to a
    /// delegating agent carries only the names its definition allows —
    /// the full registered list the delegator itself sees is not what the
    /// nested turn gets.
    #[test]
    fn a_subagents_allowlist_narrows_the_offered_spec() {
        let all = vec![
            crate::agents::AgentDef {
                name: "explorer".to_string(),
                description: "Explores.".to_string(),
                tools: None,
                subagents: None,
                prompt: "You explore.".to_string(),
                profile: None,
            },
            crate::agents::AgentDef {
                name: "other-agent".to_string(),
                description: "Does other things.".to_string(),
                tools: None,
                subagents: None,
                prompt: "You do other things.".to_string(),
                profile: None,
            },
        ];
        let delegator = crate::agents::AgentDef {
            tools: None,
            subagents: Some(vec!["explorer".to_string()]),
            ..all[0].clone()
        };
        // The delegator itself sees both agents; its nested turn must see
        // only the one its allowlist names.
        let parent_visible = [spec_named(SUBAGENT_TOOL_NAME)];
        let inherited = subagent_tool_specs(&delegator, &parent_visible, &all, 0, 2);
        assert_eq!(inherited.len(), 1, "{inherited:?}");
        let desc = inherited[0].description.as_ref();
        assert!(desc.contains("explorer: "), "{desc}");
        assert!(
            !desc.contains("other-agent:"),
            "narrowed spec leaked a non-allowlisted agent: {desc}"
        );
    }

    /// `subagents: []` is an explicit refusal, not an omission: even a
    /// turn comfortably below the cap gets no `subagent` at all.
    #[test]
    fn an_empty_subagents_list_forbids_delegation_at_any_depth() {
        let def = crate::agents::AgentDef {
            tools: None,
            subagents: Some(vec![]),
            ..defs()[0].clone()
        };
        let parent_visible = [spec_named(SUBAGENT_TOOL_NAME)];
        assert!(subagent_tool_specs(&def, &parent_visible, &defs(), 0, 2).is_empty());
    }

    /// `subagent` named in a definition's own `tools:` is not a typo and
    /// is not a grant: the filter never passes it through as an inherited
    /// tool. (Below the depth cap the tool can still reach the nested
    /// turn — re-appended by the depth machinery, not by this list; see
    /// `the_subagent_tool_is_offered_only_below_the_depth_cap`. At
    /// `max_depth = 0` no delegation is possible at any depth, so the
    /// filter's verdict here is the whole story.)
    #[test]
    fn a_definition_cannot_grant_itself_subagent() {
        let greedy = crate::agents::AgentDef {
            tools: Some(vec![
                SUBAGENT_TOOL_NAME.to_string(),
                "file_read".to_string(),
            ]),
            profile: None,
            ..defs()[0].clone()
        };
        let parent_visible = [spec_named("file_read"), spec_named(SUBAGENT_TOOL_NAME)];
        let inherited = subagent_tool_specs(&greedy, &parent_visible, &defs(), 0, 0);
        assert!(!inherited.iter().any(|s| s.name == SUBAGENT_TOOL_NAME));
    }

    /// A delegate is not the character speaking: the avatar tools the
    /// parent turn was offered never reach it, inherited or named.
    #[test]
    fn a_subagent_is_never_offered_the_avatar_tools() {
        let parent_visible = [
            spec_named("file_read"),
            spec_named("set_expression"),
            spec_named("play_action"),
        ];
        let unrestricted = crate::agents::AgentDef {
            tools: None,
            ..defs()[0].clone()
        };
        let naming = crate::agents::AgentDef {
            tools: Some(vec!["set_expression".to_string(), "file_read".to_string()]),
            ..defs()[0].clone()
        };
        for def in [unrestricted, naming] {
            let names: Vec<_> = subagent_tool_specs(&def, &parent_visible, &defs(), 0, 0)
                .into_iter()
                .map(|s| s.name.to_string())
                .collect();
            assert_eq!(names, ["file_read"]);
        }
    }

    /// An empty list is a definition, not an omission.
    #[test]
    fn an_empty_tools_list_yields_no_tools() {
        let toolless = crate::agents::AgentDef {
            tools: Some(vec![]),
            profile: None,
            ..defs()[0].clone()
        };
        let parent_visible = [spec_named("file_read")];
        assert!(subagent_tool_specs(&toolless, &parent_visible, &defs(), 0, 2).is_empty());
    }

    /// A nested turn has no session to deliver a background command
    /// into, so the `shell` it inherits has no `background`.
    #[test]
    fn a_nested_shell_spec_has_no_background() {
        let unrestricted = crate::agents::AgentDef {
            tools: None,
            ..defs()[0].clone()
        };
        let parent_visible = [crate::tools::builtin_tools::shell_spec(true)];
        let inherited = subagent_tool_specs(&unrestricted, &parent_visible, &defs(), 0, 2);
        let shell = inherited
            .iter()
            .find(|s| s.name == "shell")
            .expect("`shell` is inherited");
        assert!(
            shell.input_schema["properties"].get("background").is_none(),
            "{}",
            shell.input_schema
        );
        assert!(shell.input_schema["properties"].get("command").is_some());
    }

    fn spec_named(name: &str) -> crate::provider::ToolSpec {
        crate::provider::ToolSpec {
            name: name.to_string().into(),
            description: "…".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    /// A name in `tools:` that the parent cannot see is reported once —
    /// not on every delegation — and a name that *is* visible is never
    /// reported at all.
    #[test]
    fn an_unknown_tool_name_is_reported_once() {
        let tool = SubagentTool::new(defs(), false);
        let def = agent(&tool, 0); // tools: Some(["file_read"])
        let parent_visible = [spec_named("file_read")];

        let unknown = crate::agents::AgentDef {
            tools: Some(vec!["file_read".to_string(), "retrieve".to_string()]),
            profile: None,
            ..def.clone()
        };

        let first = tool.newly_unknown_tools(&unknown, &parent_visible);
        assert_eq!(first, vec!["retrieve".to_string()], "{first:?}");

        let second = tool.newly_unknown_tools(&unknown, &parent_visible);
        assert!(
            second.is_empty(),
            "the same (agent, name) pair must not be reported twice: {second:?}"
        );
    }

    /// `subagent` named in a definition's own `tools:` is not a typo —
    /// `subagent_tool_specs` always drops it on purpose — so it must
    /// never show up as an "unknown tool" warning.
    #[test]
    fn subagent_itself_is_never_reported_as_unknown() {
        let tool = SubagentTool::new(
            vec![crate::agents::AgentDef {
                tools: Some(vec![SUBAGENT_TOOL_NAME.to_string()]),
                profile: None,
                ..defs()[0].clone()
            }],
            false,
        );
        let def = agent(&tool, 0);
        assert!(tool.newly_unknown_tools(&def, &[]).is_empty());
    }

    /// An unrestricted definition (`tools: None`) has nothing to check
    /// against the parent's visible set — there is no list to contain a
    /// typo.
    #[test]
    fn an_unrestricted_definition_has_nothing_to_warn_about() {
        let tool = SubagentTool::new(
            vec![crate::agents::AgentDef {
                tools: None,
                profile: None,
                ..defs()[0].clone()
            }],
            false,
        );
        let def = agent(&tool, 0);
        assert!(tool.newly_unknown_tools(&def, &[]).is_empty());
    }

    /// A minimal `TurnHost` that records every call it receives, so a
    /// test can tell `SubagentHost` actually forwards to the
    /// wrapped host rather than silently no-op'ing everything.
    #[derive(Default)]
    struct RecordingHost {
        turn_errors: std::sync::Mutex<Vec<String>>,
        tool_starts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::serve::TurnHost for RecordingHost {
        async fn tool_start(&self, id: &str, _name: &str, _input: &serde_json::Value) {
            self.tool_starts.lock().unwrap().push(id.to_string());
        }
        async fn tool_end(&self, _id: &str, _name: &str) {}
        async fn turn_error(&self, message: &str) {
            self.turn_errors.lock().unwrap().push(message.to_string());
        }
        fn origin(&self) -> crate::tools::policy::Origin {
            crate::tools::policy::Origin::Channel
        }
    }

    /// The one behaviour this wrapper exists to change: `turn_error`
    /// never reaches the wrapped host, so a subagent's provider failure
    /// cannot masquerade as the parent turn's own terminal outcome (see
    /// the type doc — this is Fix 2's regression test).
    #[tokio::test]
    async fn turn_error_is_swallowed_not_forwarded() {
        let inner = std::sync::Arc::new(RecordingHost::default());
        let wrapped = SubagentHost(inner.clone());

        crate::serve::TurnHost::turn_error(&wrapped, "the subagent's provider broke").await;

        assert!(
            inner.turn_errors.lock().unwrap().is_empty(),
            "turn_error must not reach the parent's host"
        );
    }

    /// サブエージェントの散文は親のストリームに漏れない。漏れると、
    /// 委任先が言ったことが親エージェント自身の発言として編集画面に出る
    /// —— 誤帰属であり、報告はツール結果として戻ってくる。
    #[tokio::test]
    async fn a_subagents_prose_does_not_reach_the_parents_stream() {
        #[derive(Default)]
        struct ChunkRecorder {
            chunks: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl crate::serve::TurnHost for ChunkRecorder {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &serde_json::Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            async fn message_chunk(&self, text: &str) {
                self.chunks.lock().unwrap().push(text.to_string());
            }
        }

        let parent = std::sync::Arc::new(ChunkRecorder::default());
        let wrapped = SubagentHost(
            std::sync::Arc::clone(&parent) as std::sync::Arc<dyn crate::serve::TurnHost>
        );

        crate::serve::TurnHost::message_chunk(&wrapped, "the subagent's own words").await;

        let seen = parent.chunks.lock().unwrap().clone();
        assert!(seen.is_empty(), "親には届かない: {seen:?}");
    }

    /// 親が無制限でも、サブエージェントは有限で回る。委譲していたら
    /// 入れ子が無制限になり、上限が二乗で消える。
    #[test]
    fn a_subagent_is_unattended_even_under_an_interactive_parent() {
        struct InteractiveParent;
        #[async_trait]
        impl crate::serve::TurnHost for InteractiveParent {
            async fn tool_start(&self, _id: &str, _name: &str, _input: &serde_json::Value) {}
            async fn tool_end(&self, _id: &str, _name: &str) {}
            async fn turn_error(&self, _message: &str) {}
            fn round_budget(&self) -> crate::serve::RoundBudget {
                crate::serve::RoundBudget::Interactive
            }
        }

        let wrapped =
            SubagentHost(std::sync::Arc::new(InteractiveParent)
                as std::sync::Arc<dyn crate::serve::TurnHost>);

        assert_eq!(
            crate::serve::TurnHost::round_budget(&wrapped),
            crate::serve::RoundBudget::Unattended
        );
    }

    /// Every other method forwards unchanged — delegation must stay
    /// inside the same permission gate the parent itself is judged by.
    #[tokio::test]
    async fn every_other_method_forwards_to_the_parent_host() {
        let inner = std::sync::Arc::new(RecordingHost::default());
        let wrapped = SubagentHost(inner.clone());

        assert_eq!(
            crate::serve::TurnHost::origin(&wrapped),
            crate::tools::policy::Origin::Channel,
            "origin() must be the parent's own, unchanged"
        );

        crate::serve::TurnHost::tool_start(&wrapped, "call-1", "some_tool", &json!({})).await;
        assert_eq!(
            inner.tool_starts.lock().unwrap().as_slice(),
            ["call-1".to_string()],
            "tool_start must still reach the parent's host"
        );
    }

    /// A parent turn with an editor on the other end — the one thing
    /// `turn_context()`'s `NullProgress` cannot stand in for, since its
    /// own `acp_client()` answers `None` (the `TurnHost` default).
    struct AcpParentHost {
        client: std::sync::Arc<dyn crate::tools::acp_client::AcpClient>,
    }

    #[async_trait]
    impl crate::serve::TurnHost for AcpParentHost {
        async fn tool_start(&self, _id: &str, _name: &str, _input: &serde_json::Value) {}
        async fn tool_end(&self, _id: &str, _name: &str) {}
        async fn turn_error(&self, _message: &str) {}
        fn acp_client(&self) -> Option<std::sync::Arc<dyn crate::tools::acp_client::AcpClient>> {
            Some(std::sync::Arc::clone(&self.client))
        }
    }

    /// A subagent delegated from an ACP session reaches the same machine
    /// its parent does. `SubagentHost::acp_client()` forwards to the
    /// parent's host, and `TurnLoop::run` is what scopes the task-local —
    /// so this is the assertion that delegation cannot silently become
    /// "the agent's own disk instead of the editor's".
    #[tokio::test]
    async fn a_delegated_subagents_tool_calls_still_reach_the_editor() {
        let client = std::sync::Arc::new(crate::tools::acp_client::tests::FakeClient::default());
        let as_client = std::sync::Arc::clone(&client)
            as std::sync::Arc<dyn crate::tools::acp_client::AcpClient>;
        let parent: std::sync::Arc<dyn crate::serve::TurnHost> =
            std::sync::Arc::new(AcpParentHost {
                client: std::sync::Arc::clone(&as_client),
            });
        // Exactly what `run_and_store` wraps the parent's host in before
        // driving a nested turn — so what the closures below read is
        // what a dispatched child's tools read.
        let childs_host = SubagentHost(std::sync::Arc::clone(&parent));

        let reaches_editor = crate::serve::scope_turn_context(
            turn_context_with_host(
                crate::serve::ServeState::for_test(false),
                ScriptedProvider::new(vec![text_response("answer")])
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::clone(&parent),
            ),
            crate::tools::acp_client::scope_acp_client(std::sync::Arc::clone(&as_client), async {
                // The delegating turn's own `TurnHost::acp_client` is what a
                // subagent's `SubagentHost` forwards...
                let forwarded = crate::serve::TurnHost::acp_client(&childs_host).is_some();
                // ...and `current_acp_client()` is what the unified
                // `file_read`/`shell` tools actually read at execution
                // time. Both halves have to hold for a child's call to land
                // on the editor's machine.
                let scoped = crate::tools::acp_client::current_acp_client().is_some();
                forwarded && scoped
            }),
        )
        .await;
        assert!(
            reaches_editor,
            "a delegated child's tool call must reach the editor, not the agent's own disk"
        );
    }

    // -----------------------------------------------------------------
    // Background subagents
    // -----------------------------------------------------------------

    /// A parent host whose `approve` would say yes to anything, so a
    /// background host that reached it would let the call run. Its
    /// `standing_approval` answers whatever the test sets.
    struct StandingParent {
        standing: Option<crate::tools::policy::Approval>,
        tool_starts: std::sync::Mutex<Vec<String>>,
        asked: std::sync::atomic::AtomicBool,
    }

    impl StandingParent {
        fn new(standing: Option<crate::tools::policy::Approval>) -> Self {
            Self {
                standing,
                tool_starts: std::sync::Mutex::new(Vec::new()),
                asked: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl crate::serve::TurnHost for StandingParent {
        async fn tool_start(&self, id: &str, _name: &str, _input: &serde_json::Value) {
            self.tool_starts.lock().unwrap().push(id.to_string());
        }
        async fn tool_end(&self, _id: &str, _name: &str) {}
        async fn turn_error(&self, _message: &str) {}
        fn origin(&self) -> crate::tools::policy::Origin {
            crate::tools::policy::Origin::Acp(crate::tools::policy::SessionMode::Default)
        }
        async fn standing_approval(
            &self,
            _call: &crate::provider::ToolCall,
            _kind: ToolKind,
        ) -> Option<crate::tools::policy::Approval> {
            self.standing
        }
        async fn approve(
            &self,
            _call: &crate::provider::ToolCall,
            _kind: ToolKind,
        ) -> crate::tools::policy::Approval {
            self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::tools::policy::Approval::AllowOnce
        }
    }

    fn background_host(
        parent: std::sync::Arc<StandingParent>,
    ) -> (
        BackgroundSubagentHost,
        std::sync::Arc<std::sync::Mutex<Vec<crate::background::RefusedCall>>>,
    ) {
        let refused = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let host = BackgroundSubagentHost::new(
            parent as std::sync::Arc<dyn crate::serve::TurnHost>,
            std::sync::Arc::clone(&refused),
            "h-1".to_string(),
        );
        (host, refused)
    }

    #[test]
    fn background_child_forwards_identity_with_its_handle() {
        use crate::serve::{AutonomousHost, ConsultIdentity, NullProgress, TurnHost};
        let parent_id = ConsultIdentity {
            task: "t".into(),
            owner_session: "s".into(),
            asked_by: None,
        };
        let parent: std::sync::Arc<dyn TurnHost> = std::sync::Arc::new(AutonomousHost {
            origin: crate::tools::policy::Origin::Trusted,
            consult: Some(parent_id.clone()),
        });
        let host = BackgroundSubagentHost::new(parent, Default::default(), "h-9".into());
        assert_eq!(
            host.consult_identity(),
            Some(ConsultIdentity {
                asked_by: Some("h-9".into()),
                ..parent_id
            })
        );
        let host = BackgroundSubagentHost::new(
            std::sync::Arc::new(NullProgress),
            Default::default(),
            "h-9".into(),
        );
        assert_eq!(host.consult_identity(), None);
    }

    fn shell_call() -> crate::provider::ToolCall {
        crate::provider::ToolCall {
            id: "bg-call-1".to_string(),
            name: "shell".to_string(),
            input: json!({"command": "rm -rf build"}),
        }
    }

    /// Nobody can answer a dialog for a background task, so a call with
    /// no standing answer is refused — without asking the parent, whose
    /// `approve` would put a prompt in front of the user (or here, say
    /// yes) — and kept for the completion report.
    #[tokio::test]
    async fn background_host_refuses_an_ask_and_records_it() {
        let parent = std::sync::Arc::new(StandingParent::new(None));
        let (host, refused) = background_host(std::sync::Arc::clone(&parent));

        let answer = crate::serve::TurnHost::approve(&host, &shell_call(), ToolKind::Execute).await;

        assert_eq!(answer, crate::tools::policy::Approval::RejectOnce);
        assert!(
            !parent.asked.load(std::sync::atomic::Ordering::SeqCst),
            "a background host must never fall through to the parent's dialog"
        );
        assert_eq!(
            refused.lock().unwrap().as_slice(),
            [crate::background::RefusedCall {
                name: "shell".to_string(),
                input: json!({"command": "rm -rf build"}),
            }]
        );
        assert_eq!(
            crate::serve::TurnHost::declined_refusal(&host, answer),
            crate::tools::policy::Refusal::NeedsApproval
        );
    }

    /// A standing "always reject" is the user's own answer, already
    /// given: the call does not run, and it is neither reported as
    /// waiting on them nor explained to the child as "needs approval" —
    /// inviting an approval the user chose never to give would
    /// contradict that choice.
    #[tokio::test]
    async fn background_host_refuses_a_standing_reject() {
        let parent = std::sync::Arc::new(StandingParent::new(Some(
            crate::tools::policy::Approval::RejectAlways,
        )));
        let (host, refused) = background_host(std::sync::Arc::clone(&parent));

        let answer = crate::serve::TurnHost::approve(&host, &shell_call(), ToolKind::Execute).await;

        assert_eq!(answer, crate::tools::policy::Approval::RejectAlways);
        assert!(!parent.asked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(refused.lock().unwrap().is_empty());
        assert_eq!(
            crate::serve::TurnHost::declined_refusal(&host, answer),
            crate::tools::policy::Refusal::UserDeclined
        );
    }

    /// A background child's own synchronous delegation wraps the
    /// background host in a `SubagentHost`; the grandchild's unstored
    /// `Ask` must still read "needs approval", not "the user declined".
    #[tokio::test]
    async fn a_subagent_host_over_a_background_host_reads_needs_approval() {
        use crate::tools::policy::{Refusal, refusal_message};
        let parent = std::sync::Arc::new(StandingParent::new(None));
        let (host, refused) = background_host(std::sync::Arc::clone(&parent));
        let nested = SubagentHost(std::sync::Arc::new(host));

        let answer =
            crate::serve::TurnHost::approve(&nested, &shell_call(), ToolKind::Execute).await;

        assert_eq!(
            refusal_message(
                "shell",
                crate::serve::TurnHost::declined_refusal(&nested, answer)
            ),
            refusal_message("shell", Refusal::NeedsApproval)
        );
        assert_eq!(refused.lock().unwrap().len(), 1);
    }

    /// What the user already told the editor to always allow runs in the
    /// background as it would in the foreground, and is not reported.
    #[tokio::test]
    async fn background_host_runs_a_standing_allow() {
        let parent = std::sync::Arc::new(StandingParent::new(Some(
            crate::tools::policy::Approval::AllowAlways,
        )));
        let (host, refused) = background_host(std::sync::Arc::clone(&parent));

        let answer = crate::serve::TurnHost::approve(&host, &shell_call(), ToolKind::Execute).await;

        assert!(answer.allows());
        assert!(!parent.asked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(refused.lock().unwrap().is_empty());
    }

    /// The permission table is the parent's own, but the parent's tool
    /// events are not: a background child's calls are not part of the
    /// turn the editor is showing.
    #[tokio::test]
    async fn background_host_forwards_origin_but_not_tool_events() {
        let parent = std::sync::Arc::new(StandingParent::new(None));
        let (host, _refused) = background_host(std::sync::Arc::clone(&parent));

        assert_eq!(
            crate::serve::TurnHost::origin(&host),
            crate::tools::policy::Origin::Acp(crate::tools::policy::SessionMode::Default)
        );
        crate::serve::TurnHost::tool_start(&host, "bg-call-1", "shell", &json!({})).await;
        assert!(
            parent.tool_starts.lock().unwrap().is_empty(),
            "tool_start must not reach the parent's host"
        );
        assert_eq!(
            crate::serve::TurnHost::round_budget(&host),
            crate::serve::RoundBudget::Unattended
        );
    }

    // -----------------------------------------------------------------
    // Resume (Task 6)
    // -----------------------------------------------------------------

    /// Definitions carrying two tools, so the depth-cap test below can
    /// tell "the offered list was recomputed" apart from "the offered
    /// list happened to be empty".
    fn resumable_defs() -> Vec<crate::agents::AgentDef> {
        vec![crate::agents::AgentDef {
            name: "impl".to_string(),
            description: "Implements a task.".to_string(),
            tools: Some(vec!["file_read".to_string(), "shell".to_string()]),
            subagents: None,
            prompt: "You are impl.".to_string(),
            profile: None,
        }]
    }

    fn text_response(text: &str) -> crate::provider::ChatResponse {
        crate::provider::ChatResponse {
            prompt_usage: None,
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        }
    }

    /// Pulls the handle out of a `[subagent <name> · handle <handle>]`
    /// prefix — the exact format `prefixed` produces for a resumable
    /// answer.
    fn extract_handle(answer: &str) -> String {
        let after = answer
            .split("handle ")
            .nth(1)
            .unwrap_or_else(|| panic!("no handle in: {answer}"));
        after
            .split(']')
            .next()
            .unwrap_or_else(|| panic!("unterminated handle in: {answer}"))
            .to_string()
    }

    /// A provider double that records the full `(system, messages,
    /// tool_specs)` of every `chat()` call, not just the scripted
    /// outcome.
    ///
    /// `serve::StubProvider`'s own `ChatLog` (used by the depth-cap test
    /// in `src/serve/mod.rs`) only records tool *names*, which is enough
    /// to prove a call was offered a given set of tools but says
    /// nothing about message *content* — so it cannot answer "did the
    /// resumed turn's provider call actually carry its own stored
    /// history, and none of whatever else was in scope". This exists
    /// for that, kept local to this module rather than extending
    /// `ChatLog` for one property only these tests need.
    ///
    /// `chat()` yields once before answering (`tokio::task::yield_now`),
    /// the same technique `acp_client::tests::FakeClient::create_terminal`
    /// uses: a fake that resolved synchronously would let one resume's
    /// entire call — claim, run, persist, release — complete within a
    /// single poll, never giving a concurrently-started second resume a
    /// chance to observe the busy guard still held.
    /// One call the provider saw: system prompt, messages, tools.
    type ProviderCall = (Option<String>, Vec<ChatMessage>, Vec<ToolSpec>);

    #[derive(Default)]
    struct ScriptedProvider {
        script: std::sync::Mutex<std::collections::VecDeque<crate::provider::ChatResponse>>,
        calls: std::sync::Mutex<Vec<ProviderCall>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<crate::provider::ChatResponse>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                script: std::sync::Mutex::new(script.into()),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// The `messages` the most recent `chat()` call received.
        fn last_messages(&self) -> Vec<ChatMessage> {
            self.calls
                .lock()
                .unwrap()
                .last()
                .expect("a call was recorded")
                .1
                .clone()
        }

        /// The `tool_specs` the most recent `chat()` call received.
        fn last_specs(&self) -> Vec<ToolSpec> {
            self.calls
                .lock()
                .unwrap()
                .last()
                .expect("a call was recorded")
                .2
                .clone()
        }

        /// How many `chat()` calls this double received. `last_messages`
        /// panics on an empty log by design; "this provider received
        /// nothing" needs a count instead.
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl crate::provider::Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn chat(
            &self,
            system: Option<&str>,
            messages: &[ChatMessage],
            tools: Option<&[ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            self.calls.lock().unwrap().push((
                system.map(|s| s.to_string()),
                messages.to_vec(),
                tools.map(|t| t.to_vec()).unwrap_or_default(),
            ));
            tokio::task::yield_now().await;
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("ScriptedProvider script exhausted"))
        }
    }

    #[tokio::test]
    async fn owner_scope_is_cleared_in_child_turn() {
        use crate::tools::attachment_tools::{current_attachment_owner, scope_attachment_owner};
        struct OwnerCheckingProvider;
        #[async_trait]
        impl crate::provider::Provider for OwnerCheckingProvider {
            fn name(&self) -> &str {
                "owner-check"
            }
            async fn chat(
                &self,
                _: Option<&str>,
                _: &[ChatMessage],
                _: Option<&[ToolSpec]>,
            ) -> anyhow::Result<crate::provider::ChatResponse> {
                assert_eq!(
                    current_attachment_owner(),
                    None,
                    "the entire child turn must be unprivileged"
                );
                Ok(text_response("child answer"))
            }
        }
        let tool = SubagentTool::new(defs(), false);
        let owner = crate::attachment_store::AttachmentOwner {
            platform: "matrix".into(),
            room_id: "!padoc:example.org".into(),
            thread_id: None,
        };
        assert_eq!(current_attachment_owner(), None);
        scope_attachment_owner(Some(owner.clone()), async {
            let answer = crate::serve::scope_turn_context(
                turn_context(
                    crate::serve::ServeState::for_test(false),
                    std::sync::Arc::new(OwnerCheckingProvider),
                    Vec::new(),
                ),
                tool.execute(&serde_json::json!({"agent":"reviewer", "prompt":"go"})),
            )
            .await
            .unwrap();
            assert!(answer.contains("child answer"));
            assert_eq!(current_attachment_owner(), Some(owner));
        })
        .await;
        assert_eq!(current_attachment_owner(), None);
    }

    /// A `TurnContext` built by hand rather than through a real parent
    /// turn — `SubagentTool::execute` only ever reads it through
    /// `current_turn_context()`, so scoping one directly around the call
    /// under test is enough, and keeps these tests from having to drive
    /// a whole outer `run_llm_turn` just to reach the resume path.
    fn turn_context(
        state: std::sync::Arc<crate::serve::ServeState>,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        visible_specs: Vec<ToolSpec>,
    ) -> std::sync::Arc<crate::serve::TurnContext> {
        turn_context_with_host(
            state,
            provider,
            visible_specs,
            std::sync::Arc::new(crate::serve::NullProgress),
        )
    }

    /// The same context as [`turn_context`], with the turn's own host
    /// supplied rather than a [`crate::serve::NullProgress`]. That
    /// default's `acp_client()` is `None` (see `TurnHost`'s doc), so a
    /// test about `SubagentHost` forwarding the *editor* needs a host
    /// that actually has one.
    fn turn_context_with_host(
        state: std::sync::Arc<crate::serve::ServeState>,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        visible_specs: Vec<ToolSpec>,
        progress: std::sync::Arc<dyn crate::serve::TurnHost>,
    ) -> std::sync::Arc<crate::serve::TurnContext> {
        std::sync::Arc::new(crate::serve::TurnContext {
            state,
            provider,
            progress,
            visible_specs: visible_specs.into(),
            timer_origin: None,
            // `None` here matches what a subagent's own nested `TurnLoop`
            // sees by construction (see `TurnContext::session_id`'s
            // doc) — nothing under test reads it, but a stray unwrap
            // added later should fail loudly on `None`, not silently
            // pass because a test handed it a `Some`.
            session_id: None,
            subagent_depth: 0,
            admin_room_profile: None,
        })
    }

    /// A dispatched child's next resume must see its own prior history —
    /// the dispatch prompt and the model's own reply to it — appended
    /// to, not replacing, whatever it is asked next. Nothing here routes
    /// a parent's own conversation through this path at all (a
    /// subagent's history is always built from scratch on dispatch and
    /// `stored.history` alone on resume — see `dispatch`/`resume`), so
    /// this also stands as the regression test for that: if resume ever
    /// started pulling from anything other than the stored child, this
    /// assertion is where it would first show up as unexpected text.
    ///
    /// The isolation half of that used to assert `!texts.iter().any(|t|
    /// t.contains("PARENT ONLY"))` against text that appeared nowhere in
    /// the fixture (or the crate) — an assertion that could not fail
    /// regardless of what the code did. To make it a real regression
    /// test, a real parent turn's own stored history — `state.sessions`,
    /// keyed by `TurnContext::session_id`, never read by
    /// `SubagentTool` directly — is seeded with a `"PARENT ONLY"`
    /// marker and both calls run under a `TurnContext` whose
    /// `session_id` actually names that session, the way the outer call
    /// into `SubagentTool::execute` looks in production (unlike the
    /// `None` `turn_context()` helper below, which matches what a
    /// *subagent's own nested* `TurnLoop` sees, not this boundary).
    #[tokio::test]
    async fn a_resumed_child_continues_its_own_history() {
        let tool = SubagentTool::new(resumable_defs(), false);
        let state = crate::serve::ServeState::for_test(false);
        state.sessions.lock().await.insert(
            "parent-session".to_string(),
            vec![ChatMessage::user(
                "PARENT ONLY — the parent's own conversation, never sent to a subagent",
            )],
        );
        let parent_ctx = |provider: std::sync::Arc<dyn crate::provider::Provider>| {
            std::sync::Arc::new(crate::serve::TurnContext {
                state: std::sync::Arc::clone(&state),
                provider,
                progress: std::sync::Arc::new(crate::serve::NullProgress),
                visible_specs: Vec::<ToolSpec>::new().into(),
                timer_origin: None,
                session_id: Some("parent-session".to_string()),
                subagent_depth: 0,
                admin_room_profile: None,
            })
        };

        let dispatch_provider = ScriptedProvider::new(vec![text_response("dispatch answer")]);
        let dispatch_input = serde_json::json!({"agent": "impl", "prompt": "first task"});
        let dispatched = crate::serve::scope_turn_context(
            parent_ctx(std::sync::Arc::clone(&dispatch_provider)
                as std::sync::Arc<dyn crate::provider::Provider>),
            tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        let resume_provider = ScriptedProvider::new(vec![text_response("resume answer")]);
        let resume_input = serde_json::json!({"resume": handle, "prompt": "second instruction"});
        crate::serve::scope_turn_context(
            parent_ctx(std::sync::Arc::clone(&resume_provider)
                as std::sync::Arc<dyn crate::provider::Provider>),
            tool.execute(&resume_input),
        )
        .await
        .unwrap();

        let texts: Vec<String> = resume_provider
            .last_messages()
            .iter()
            .filter_map(|m| m.text())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("first task")),
            "the resumed call must see the dispatch prompt: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("dispatch answer")),
            "the resumed call must see the dispatch reply: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("second instruction")),
            "the resumed call must see its own new prompt: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("PARENT ONLY")),
            "nothing from outside the stored child history may reach a \
             resumed call: {texts:?}"
        );
    }

    /// Restoring a stored tool list on resume would make resume the
    /// hole `subagent_tool_specs` and `TurnLoop::run`'s offer gate exist
    /// to close (see the module doc's third property, and `resume`'s
    /// own doc). Asserted by full equality against exactly the two
    /// tools `resumable_defs`' `tools:` list names — not merely "no
    /// `subagent` in the list" — because a resume that happened to
    /// widen or narrow the list to anything else would pass a weaker
    /// check just as easily.
    #[tokio::test]
    async fn resume_recomputes_the_tool_list_so_the_depth_cap_still_holds() {
        let tool = SubagentTool::new(resumable_defs(), false);
        let mut state = crate::serve::ServeState::for_test(false);
        // Pin the cap to 1 (the pre-recursion "never nest" behaviour) so
        // the recomputed list has no `subagent` in it and any widening at
        // all fails the strict equality below.
        Arc::get_mut(&mut state)
            .unwrap()
            .config
            .tools
            .subagent
            .max_depth = 1;
        let state = state;

        // Wider than `resumable_defs`' own `tools:` list, and including
        // `subagent` itself, so a resumed list that was merely *not
        // empty* (rather than exactly recomputed) would still fail this
        // test.
        let visible = vec![
            spec_named("file_read"),
            spec_named("shell"),
            spec_named("some_other_tool"),
            spec_named(SUBAGENT_TOOL_NAME),
        ];

        let dispatch_provider = ScriptedProvider::new(vec![text_response("ok")]);
        let dispatch_input = serde_json::json!({"agent": "impl", "prompt": "go"});
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&dispatch_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                visible.clone(),
            ),
            tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        let resume_provider = ScriptedProvider::new(vec![text_response("ok again")]);
        let resume_input = serde_json::json!({"resume": handle, "prompt": "next"});
        crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                visible,
            ),
            tool.execute(&resume_input),
        )
        .await
        .unwrap();

        let specs = resume_provider.last_specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(names, vec!["file_read", "shell"]);
    }

    /// A handle nobody ever stored (typo'd, expired, from a different
    /// process) is recoverable the same way an unknown agent name is:
    /// told what to do instead, not just refused.
    #[tokio::test]
    async fn an_unknown_handle_is_recoverable_and_says_what_to_do() {
        let tool = SubagentTool::new(defs(), false);
        let state = crate::serve::ServeState::for_test(false);
        let provider = ScriptedProvider::new(Vec::new());
        let input = serde_json::json!({"resume": "nosuchhandle", "prompt": "x"});

        let err = crate::serve::scope_turn_context(
            turn_context(
                state,
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&input),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("dispatch"), "got: {err}");
    }

    /// A deployment with no `subagent_cache` at all must say so, not
    /// report the same "no subagent is stored under handle '…'" message
    /// `an_unknown_handle_is_recoverable_and_says_what_to_do` gets for a
    /// genuine miss against a real, empty cache — that phrasing invites
    /// retrying with a different handle, when no handle could ever
    /// resolve here.
    #[tokio::test]
    async fn resume_says_no_cache_is_configured_rather_than_a_phantom_miss() {
        let mut state = crate::serve::ServeState::for_test(false);
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .subagent_cache = None;

        let tool = SubagentTool::new(defs(), false);
        let provider = ScriptedProvider::new(Vec::new());
        let input = serde_json::json!({"resume": "some-handle", "prompt": "x"});

        let err = crate::serve::scope_turn_context(
            turn_context(
                state,
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&input),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("no resume cache is configured"), "got: {err}");
    }

    /// Two turns resuming one handle at once would interleave writes
    /// into a single stored history. `ScriptedProvider::chat`'s forced
    /// yield (see its doc) is what makes the second attempt
    /// deterministically observe the first still holding the guard,
    /// rather than depending on scheduler luck.
    #[tokio::test]
    async fn a_busy_handle_is_refused() {
        let tool = SubagentTool::new(defs(), false);
        let state = crate::serve::ServeState::for_test(false);

        let dispatch_provider = ScriptedProvider::new(vec![text_response("dispatch answer")]);
        let dispatch_input = serde_json::json!({"agent": "reviewer", "prompt": "go"});
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&dispatch_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        let provider_a = ScriptedProvider::new(vec![text_response("a")]);
        let provider_b = ScriptedProvider::new(vec![text_response("b")]);
        let input_a = serde_json::json!({"resume": handle, "prompt": "x"});
        let input_b = serde_json::json!({"resume": handle, "prompt": "y"});

        let fut_a = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider_a) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&input_a),
        );
        let fut_b = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider_b) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&input_b),
        );

        let (first, second) = tokio::join!(fut_a, fut_b);
        assert!(first.is_ok(), "{first:?}");
        assert!(
            second.unwrap_err().to_string().contains("in use"),
            "the second concurrent resume of the same handle must be refused"
        );
    }

    /// A definition reload (the operator edited the `.md`, or dropped
    /// the agent) is not the same failure as an unknown handle — the
    /// handle is real, its child answered before, but the definition it
    /// belongs to is gone. Simulated with two separate `SubagentTool`
    /// instances sharing one cache-backed `ServeState`, standing in for
    /// "the process reloaded its agent definitions between dispatch and
    /// resume" without needing a mutable, shared agents list.
    #[tokio::test]
    async fn an_agent_definition_that_disappeared_is_reported() {
        let state = crate::serve::ServeState::for_test(false);

        let dispatching_tool = SubagentTool::new(
            vec![crate::agents::AgentDef {
                name: "impl".to_string(),
                description: "Implements a task.".to_string(),
                tools: None,
                subagents: None,
                prompt: "You are impl.".to_string(),
                profile: None,
            }],
            false,
        );
        let dispatch_provider = ScriptedProvider::new(vec![text_response("dispatch answer")]);
        let dispatch_input = serde_json::json!({"agent": "impl", "prompt": "go"});
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&dispatch_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            dispatching_tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        // A `SubagentTool` whose agent list no longer has "impl" —
        // reloaded, per the module doc, from the current list, not from
        // anything stored.
        let resuming_tool = SubagentTool::new(Vec::new(), false);
        let resume_provider = ScriptedProvider::new(Vec::new());
        let resume_input = serde_json::json!({"resume": handle, "prompt": "x"});
        let err = crate::serve::scope_turn_context(
            turn_context(
                state,
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            resuming_tool.execute(&resume_input),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("impl"), "got: {err}");
    }

    /// `SubagentCache::put` refuses an over-cap history wholesale rather
    /// than truncating it (see `persist`'s doc for why truncation is
    /// not an option). The answer must still come back normally — only
    /// the resumability marker changes — so a huge answer is never
    /// silently lost just because it made the child unresumable.
    #[tokio::test]
    async fn an_over_cap_child_still_answers_but_says_it_is_not_resumable() {
        let mut state = crate::serve::ServeState::for_test(false);
        let cache_dir = tempfile::tempdir().unwrap();
        let tiny_cache =
            crate::subagent_cache::SubagentCache::open(cache_dir.path().to_path_buf(), 64).unwrap();
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .subagent_cache = Some(tiny_cache);

        let tool = SubagentTool::new(defs(), false);
        let big_answer = format!("the answer is {}", "x".repeat(10_000));
        let provider = ScriptedProvider::new(vec![text_response(&big_answer)]);
        let input = serde_json::json!({"agent": "reviewer", "prompt": "go"});

        let out = crate::serve::scope_turn_context(
            turn_context(
                state,
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&input),
        )
        .await
        .unwrap();

        assert!(out.contains("not resumable"), "got: {out}");
        assert!(out.contains("the answer"), "answer was lost: {out}");
    }

    /// Unlike `dispatch`'s over-cap case (nothing was ever on disk, so
    /// there is no earlier copy to be misled by), a resume that
    /// overflows the cap leaves the *previous*, shorter history in
    /// place — the handle keeps resolving, just not to what this call
    /// just returned. The marker has to name that divergence, not
    /// reuse dispatch's plain "history exceeded the cache limit".
    #[tokio::test]
    async fn a_resume_over_cap_names_the_divergence_not_just_the_cap() {
        let mut state = crate::serve::ServeState::for_test(false);
        let cache_dir = tempfile::tempdir().unwrap();
        let tiny_cache =
            crate::subagent_cache::SubagentCache::open(cache_dir.path().to_path_buf(), 1_000)
                .unwrap();
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .subagent_cache = Some(tiny_cache);

        let tool = SubagentTool::new(defs(), false);

        // Small enough that the dispatch itself fits comfortably under
        // the 1,000-byte cap.
        let dispatch_provider = ScriptedProvider::new(vec![text_response("dispatch ok")]);
        let dispatch_input = serde_json::json!({"agent": "reviewer", "prompt": "go"});
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&dispatch_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        assert!(
            dispatched.contains("handle"),
            "the dispatch itself must have fit under the cap, or this \
             test isn't isolating the resume-path case: {dispatched}"
        );
        let handle = extract_handle(&dispatched);

        // Large enough that appending it (plus the reply) pushes the
        // stored history over the same cap on resume.
        let resume_provider = ScriptedProvider::new(vec![text_response("resume ok")]);
        let huge_prompt = "y".repeat(5_000);
        let resume_input = serde_json::json!({"resume": handle, "prompt": huge_prompt});
        let out = crate::serve::scope_turn_context(
            turn_context(
                state,
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&resume_input),
        )
        .await
        .unwrap();

        assert!(out.contains("not resumable"), "got: {out}");
        assert!(
            out.contains("will not include"),
            "the resume-path message must name the divergence — that the \
             stored copy still ends before this exchange — not just \
             repeat dispatch's plain cap message: {out}"
        );
    }

    /// `agent`/`resume` present but not a string must be reported as
    /// what it is — a type error on that key — not silently treated the
    /// same as the key never having been given at all. See
    /// `string_field`'s doc.
    #[tokio::test]
    async fn a_non_string_agent_is_a_type_error_not_treated_as_absent() {
        let tool = SubagentTool::new(defs(), false);
        let err = tool
            .execute(&serde_json::json!({"agent": 123, "prompt": "x"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent"), "got: {err}");
        assert!(err.contains("string"), "got: {err}");
    }

    /// The bug this closes: with a bare `.and_then(|v| v.as_str())`, a
    /// present-but-wrong-typed `agent` was indistinguishable from an
    /// absent one, so `{"agent": 123, "resume": "h", ...}` would slip
    /// past the mutual-exclusivity check in `execute` and resume
    /// silently — even though `agent` was very much present. It must
    /// instead be reported as a type error before that check ever runs.
    #[tokio::test]
    async fn a_wrong_typed_agent_cannot_smuggle_a_resume_past_the_exclusivity_check() {
        let tool = SubagentTool::new(defs(), false);
        let err = tool
            .execute(&serde_json::json!({"agent": 123, "resume": "h", "prompt": "x"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("agent") && err.contains("string"),
            "must be reported as a type error on 'agent', not silently \
             resumed: got: {err}"
        );
    }

    /// `TerminalReservation` leaked its slot this exact way on an
    /// earlier branch, which is why it is a `Drop` guard rather than a
    /// remember-to-remove-the-entry pattern — see its own doc
    /// (`src/tools/acp_client.rs`) and the precedent this mirrors,
    /// `client_tools::tests::a_dropped_reservation_does_not_leak_the_slot`.
    /// Confirms `ResumeGuard` has the same property: a resume cancelled
    /// mid-flight (the future simply dropped — ACP treats that as
    /// routine, no different from an Escape in the editor or a dropped
    /// socket) still releases the handle, rather than leaving it
    /// refused forever.
    #[tokio::test]
    async fn a_cancelled_resume_releases_the_busy_guard() {
        let tool = SubagentTool::new(defs(), false);
        let state = crate::serve::ServeState::for_test(false);

        let dispatch_provider = ScriptedProvider::new(vec![text_response("dispatch answer")]);
        let dispatch_input = serde_json::json!({"agent": "reviewer", "prompt": "go"});
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&dispatch_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&dispatch_input),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        let resume_provider = ScriptedProvider::new(vec![text_response("resume answer")]);
        let resume_input = serde_json::json!({"resume": handle.clone(), "prompt": "x"});
        let ctx = turn_context(
            std::sync::Arc::clone(&state),
            std::sync::Arc::clone(&resume_provider)
                as std::sync::Arc<dyn crate::provider::Provider>,
            Vec::new(),
        );
        // Scoped in its own block rather than an explicit `drop(fut)`:
        // `tokio::pin!` shadows `fut` with a `Pin<&mut T>` pointing at a
        // second, hidden owned local holding the real future — dropping
        // the visible `fut` only drops that reference wrapper, not the
        // future itself (and, with it, the `ResumeGuard` inside), which
        // stays alive until its own (hidden) binding's scope ends. This
        // block gives it one that ends before the assertion below runs,
        // so the drop this test is checking for has actually happened
        // by the time it checks.
        {
            let fut = crate::serve::scope_turn_context(ctx, tool.execute(&resume_input));
            tokio::pin!(fut);
            assert!(futures_util::poll!(fut.as_mut()).is_pending());
            assert!(tool.busy_handles.lock().unwrap().contains(&handle));
        }
        assert!(tool.busy_handles.lock().unwrap().is_empty());
    }

    /// A definition with `profile:` runs on that profile's provider, not the
    /// parent's — the point of the feature. The fixture's registry
    /// (`serve::build_for_test_with`) registers one scripted provider under
    /// both the `"anthropic"` and `"stub"` names, so profile resolution and
    /// the ctx provider cannot be told apart by *identity* here — the
    /// distinction is made by *recorded calls*: the ctx provider is a
    /// separate record-only double whose log must stay empty.
    #[tokio::test]
    async fn a_definition_with_a_profile_runs_on_that_profiles_provider() {
        let tool = SubagentTool::new(
            vec![crate::agents::AgentDef {
                name: "impl".to_string(),
                description: "Implements a task.".to_string(),
                tools: Some(vec![]),
                subagents: None,
                prompt: "You are impl.".to_string(),
                profile: Some("dev".to_string()),
            }],
            false,
        );
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![text_response("profile answer")],
        );
        let parent_provider = ScriptedProvider::new(vec![]);
        let out = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&parent_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&serde_json::json!({"agent": "impl", "prompt": "go"})),
        )
        .await
        .unwrap();
        assert!(out.contains("profile answer"), "{out}");
        assert_eq!(
            parent_provider.call_count(),
            0,
            "the ctx (parent's) provider must not serve a profiled agent"
        );
    }

    /// The mirror image: no `profile:`, the parent's provider serves the
    /// turn — the pre-feature default is unchanged behaviour, not an
    /// accidental second path. Same fixture shape as above; here the ctx
    /// provider IS the one that must receive the call.
    #[tokio::test]
    async fn a_definition_without_a_profile_runs_on_the_parents_provider() {
        let tool = SubagentTool::new(resumable_defs(), false); // profile: None
        let state = crate::serve::ServeState::for_test(false);
        let parent_provider = ScriptedProvider::new(vec![text_response("parent answer")]);
        let out = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&parent_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&serde_json::json!({"agent": "impl", "prompt": "go"})),
        )
        .await
        .unwrap();
        assert!(out.contains("parent answer"), "{out}");
        assert_eq!(parent_provider.call_count(), 1);
    }

    /// A handle stays resumable after the definition's `profile:` changes:
    /// the cache stores model-agnostic `ChatMessage` history, so a resume
    /// simply runs on whatever provider the reloaded definition resolves
    /// to. This is the existing `an_agent_definition_that_disappeared_is_
    /// reported` pattern — two tool instances sharing one cache-backed
    /// state, standing in for "definitions reloaded between dispatch and
    /// resume" — with the one differing field being `profile`: dispatch
    /// runs with `profile: None` (the ctx provider), resume runs with
    /// `profile: Some("dev")` and must still continue the stored history.
    #[tokio::test]
    async fn a_resumed_handle_survives_a_profile_change() {
        let state = crate::serve::ServeState::for_test_scripted(
            false,
            vec![
                text_response("dispatch answer"),
                text_response("resume answer"),
            ],
        );
        let base = crate::agents::AgentDef {
            name: "impl".to_string(),
            description: "Implements a task.".to_string(),
            tools: Some(vec![]),
            subagents: None,
            prompt: "You are impl.".to_string(),
            profile: None,
        };
        let dispatched = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                state.registry.default_provider(),
                Vec::new(),
            ),
            SubagentTool::new(vec![base.clone()], false)
                .execute(&serde_json::json!({"agent": "impl", "prompt": "first task"})),
        )
        .await
        .unwrap();
        let handle = extract_handle(&dispatched);

        let resumed_def = crate::agents::AgentDef {
            profile: Some("dev".to_string()),
            ..base
        };
        let resumed = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                state.registry.default_provider(),
                Vec::new(),
            ),
            SubagentTool::new(vec![resumed_def], false)
                .execute(&serde_json::json!({"resume": handle, "prompt": "second instruction"})),
        )
        .await
        .unwrap();
        assert!(resumed.contains("resume answer"), "{resumed}");
        assert!(resumed.contains(&handle), "still resumable: {resumed}");
    }

    /// A tool whose call never returns — the other way, besides a stalled
    /// provider stream, that a subagent's turn can hang inside one round
    /// (#258).
    struct HangingTool(ToolSpec);

    #[async_trait]
    impl Tool for HangingTool {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }

        fn kind(&self) -> ToolKind {
            // Allowed from every origin, so the call actually runs — and
            // hangs — rather than being refused before it starts.
            ToolKind::Read
        }

        async fn execute(&self, _input: &serde_json::Value) -> anyhow::Result<String> {
            std::future::pending().await
        }
    }

    /// A subagent stuck inside a round must not hold its parent's call open
    /// forever, and what it did get done must not be thrown away.
    ///
    /// Stuck on a hanging tool rather than a hanging provider because that
    /// is the harder case for the saved history: the turn is dropped after
    /// the assistant's `tool_use` was pushed and before any `tool_result`
    /// was. Stored as-is, the provider API would reject the handle's very
    /// first resume — so the resume below is the real assertion, not just
    /// the error text.
    #[tokio::test]
    async fn a_subagent_turn_past_its_deadline_errors_and_stays_resumable() {
        let mut state = crate::serve::ServeState::for_test(false);
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .config
            .tools
            .subagent
            .turn_timeout_secs = 1;
        let hang_spec = spec_named("hang");
        state
            .tools
            .register_tool(Box::new(HangingTool(hang_spec.clone())))
            .await;

        // Unrestricted, so `hang` is actually offered — `defs()`' reviewer
        // would refuse it as not in its `tools:` list and never hang at all.
        let tool = SubagentTool::new(
            vec![crate::agents::AgentDef {
                name: "worker".to_string(),
                description: "Works.".to_string(),
                tools: None,
                subagents: None,
                prompt: "You are a worker.".to_string(),
                profile: None,
            }],
            false,
        );
        let provider = ScriptedProvider::new(vec![crate::provider::ChatResponse {
            prompt_usage: None,
            text: None,
            tool_calls: vec![crate::provider::ToolCall {
                id: "call_hang".to_string(),
                name: "hang".to_string(),
                input: serde_json::json!({}),
            }],
            stop_reason: None,
        }]);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::serve::scope_turn_context(
                turn_context(
                    std::sync::Arc::clone(&state),
                    std::sync::Arc::clone(&provider)
                        as std::sync::Arc<dyn crate::provider::Provider>,
                    vec![hang_spec.clone()],
                ),
                tool.execute(&serde_json::json!({"agent": "worker", "prompt": "go"})),
            ),
        )
        .await
        .expect("the subagent's own deadline must fire before the test's")
        .expect_err("a turn past its deadline is an error, not an answer")
        .to_string();
        assert!(err.contains("timed out"), "got: {err}");
        let handle = err
            .split("pass handle ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("the error must carry a resumable handle: {err}"))
            .to_string();

        let resume_provider = ScriptedProvider::new(vec![text_response("picked back up")]);
        let resumed = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                vec![hang_spec],
            ),
            tool.execute(&serde_json::json!({"resume": handle, "prompt": "carry on"})),
        )
        .await
        .unwrap();
        assert!(resumed.contains("picked back up"), "got: {resumed}");

        // The interrupted call is answered in the very next message, which
        // is the adjacency the API validates.
        let messages = resume_provider.last_messages();
        let use_at = messages
            .iter()
            .position(|m| {
                m.parts.iter().any(|p| {
                    matches!(p, crate::provider::ContentPart::ToolUse { id, .. } if id == "call_hang")
                })
            })
            .expect("the interrupted tool_use is part of the saved history");
        assert!(
            messages
                .get(use_at + 1)
                .is_some_and(|next| next.parts.iter().any(|p| {
                    matches!(
                        p,
                        crate::provider::ContentPart::ToolResult { tool_use_id, .. }
                            if tool_use_id == "call_hang"
                    )
                })),
            "the interrupted tool_use must be answered by the next message: {messages:?}"
        );
    }

    /// A provider that always fails with a fixed message — what proves a
    /// subagent's own provider failure (a rate limit, insufficient API
    /// credit, a network error) reaches the parent as the `subagent`
    /// call's own error text, not folded into a generic answer.
    struct FailingProvider {
        message: &'static str,
    }

    #[async_trait]
    impl crate::provider::Provider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            anyhow::bail!("{}", self.message)
        }
    }

    /// A subagent's own provider failure must reach the parent as the
    /// `subagent` call's own error, carrying the provider's actual message
    /// — not `answer_text`'s generic `"[the subagent produced no answer]"`,
    /// which was indistinguishable from a subagent that simply gave up.
    /// Regression test for `TurnStop::ProviderError` gaining its own
    /// `message` and `run_and_store` routing it through `provider_error`
    /// instead of `answer_text`.
    #[tokio::test]
    async fn a_providers_error_reaches_the_parent_as_a_tool_error_and_stays_resumable() {
        let state = crate::serve::ServeState::for_test(false);
        let tool = SubagentTool::new(defs(), false);
        let provider = std::sync::Arc::new(FailingProvider {
            message: "insufficient credits",
        });

        let err = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                provider as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&serde_json::json!({"agent": "reviewer", "prompt": "go"})),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("insufficient credits"),
            "the parent must see the provider's own error: {err}"
        );
        let handle = err
            .split("pass handle ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("the error must carry a resumable handle: {err}"))
            .to_string();

        // The failed dispatch is not a dead end: resuming it with a
        // provider that actually answers picks up right where it left off.
        let resume_provider = ScriptedProvider::new(vec![text_response("recovered")]);
        let resumed = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            tool.execute(&serde_json::json!({"resume": handle, "prompt": "try again"})),
        )
        .await
        .unwrap();
        assert!(resumed.contains("recovered"), "got: {resumed}");

        let texts: Vec<String> = resume_provider
            .last_messages()
            .iter()
            .filter_map(|m| m.text())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("go")),
            "the resumed call must still see the original dispatch prompt, \
             unlost by the failed first attempt: {texts:?}"
        );
    }

    // -----------------------------------------------------------------
    // `background: true`
    // -----------------------------------------------------------------

    /// A provider that announces each `chat()` call on `entered` and then
    /// waits on `gate`, so a test can hold a background child mid-turn
    /// and know it got there. `Notify::notify_one` keeps a permit when
    /// nobody is waiting yet, so neither side can miss the other.
    struct GatedProvider {
        inner: std::sync::Arc<ScriptedProvider>,
        entered: tokio::sync::Notify,
        gate: tokio::sync::Notify,
    }

    impl GatedProvider {
        fn new(script: Vec<crate::provider::ChatResponse>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                inner: ScriptedProvider::new(script),
                entered: tokio::sync::Notify::new(),
                gate: tokio::sync::Notify::new(),
            })
        }

        async fn wait_entered(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.entered.notified())
                .await
                .expect("the background child never reached its provider");
        }
    }

    #[async_trait]
    impl crate::provider::Provider for GatedProvider {
        fn name(&self) -> &str {
            "gated"
        }

        async fn chat(
            &self,
            system: Option<&str>,
            messages: &[ChatMessage],
            tools: Option<&[ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            self.entered.notify_one();
            self.gate.notified().await;
            self.inner.chat(system, messages, tools).await
        }
    }

    /// One provider for a whole parent turn and the child it starts,
    /// split by system prompt: a call whose system prompt is the child's
    /// definition body goes to `child`, anything else to `parent`. Without
    /// it the two would race for one script.
    struct ByPromptProvider {
        child_prompt: String,
        child: std::sync::Arc<dyn crate::provider::Provider>,
        parent: std::sync::Arc<dyn crate::provider::Provider>,
    }

    #[async_trait]
    impl crate::provider::Provider for ByPromptProvider {
        fn name(&self) -> &str {
            "by-prompt"
        }

        async fn chat(
            &self,
            system: Option<&str>,
            messages: &[ChatMessage],
            tools: Option<&[ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            if system == Some(self.child_prompt.as_str()) {
                self.child.chat(system, messages, tools).await
            } else {
                self.parent.chat(system, messages, tools).await
            }
        }
    }

    /// A tool of kind `Other`, which `Acp(Default)` sends to `approve`.
    struct OtherTool(ToolSpec);

    #[async_trait]
    impl Tool for OtherTool {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }

        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }

        async fn execute(&self, _input: &serde_json::Value) -> anyhow::Result<String> {
            Ok("ran".to_string())
        }
    }

    fn worker_defs() -> Vec<crate::agents::AgentDef> {
        vec![crate::agents::AgentDef {
            name: "worker".to_string(),
            description: "Works.".to_string(),
            tools: None,
            subagents: None,
            prompt: "You are a worker.".to_string(),
            profile: None,
        }]
    }

    /// [`turn_context_with_host`] with the session and depth supplied:
    /// background runs only on a main turn (`session_id` set, depth 0).
    fn background_context(
        state: std::sync::Arc<crate::serve::ServeState>,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        visible_specs: Vec<ToolSpec>,
        progress: std::sync::Arc<dyn crate::serve::TurnHost>,
        session_id: Option<&str>,
        subagent_depth: u32,
    ) -> std::sync::Arc<crate::serve::TurnContext> {
        std::sync::Arc::new(crate::serve::TurnContext {
            state,
            provider,
            progress,
            visible_specs: visible_specs.into(),
            timer_origin: None,
            session_id: session_id.map(str::to_string),
            subagent_depth,
            admin_room_profile: None,
        })
    }

    fn bg_input(agent: &str, prompt: &str) -> serde_json::Value {
        json!({"agent": agent, "prompt": prompt, "background": true})
    }

    /// The handle out of a "Started background task …" result.
    fn started_handle(started: &str) -> String {
        started
            .split("(handle ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap_or_else(|| panic!("no handle in: {started}"))
            .to_string()
    }

    /// The session's next completion, waiting for it to arrive.
    async fn next_completion(
        state: &crate::serve::ServeState,
        session: &str,
    ) -> crate::background::Completion {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(c) = state.background.drain(session).pop() {
                return c;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no background completion arrived for {session}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// An autonomous turn's host, able to ask in the consult room.
    fn asking_host() -> std::sync::Arc<dyn crate::serve::TurnHost> {
        std::sync::Arc::new(crate::serve::AutonomousHost {
            origin: crate::tools::policy::Origin::Trusted,
            consult: Some(crate::serve::ConsultIdentity {
                task: "t".into(),
                owner_session: "s-ask".into(),
                asked_by: None,
            }),
        })
    }

    fn offers_ask_human(specs: &[ToolSpec]) -> bool {
        specs
            .iter()
            .any(|s| s.name.as_ref() == crate::tools::ask_human::NAME)
    }

    /// A synchronous child can't ask (its host has no identity, so every
    /// call would refuse); it is not offered `ask_human` at all.
    #[tokio::test]
    async fn a_sync_child_is_not_offered_ask_human() {
        let state = crate::serve::ServeState::for_test(false);
        let provider = ScriptedProvider::new(vec![text_response("done")]);
        let tool = std::sync::Arc::new(SubagentTool::new(worker_defs(), true));
        crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                vec![
                    spec_named("file_read"),
                    spec_named(crate::tools::ask_human::NAME),
                ],
                asking_host(),
                Some("s-ask"),
                0,
            ),
            Tool::execute(&tool, &json!({"agent": "worker", "prompt": "go"})),
        )
        .await
        .unwrap();
        let specs = provider.last_specs();
        assert!(!offers_ask_human(&specs), "{specs:?}");
        assert!(specs.iter().any(|s| s.name.as_ref() == "file_read"));
    }

    /// A background child asks for the session that started it, so it
    /// keeps `ask_human`.
    #[tokio::test]
    async fn a_background_child_keeps_ask_human() {
        let state = crate::serve::ServeState::for_test(false);
        let provider = ScriptedProvider::new(vec![text_response("done")]);
        let tool = std::sync::Arc::new(SubagentTool::new(worker_defs(), true));
        crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                vec![
                    spec_named("file_read"),
                    spec_named(crate::tools::ask_human::NAME),
                ],
                asking_host(),
                Some("s-ask"),
                0,
            ),
            Tool::execute(&tool, &bg_input("worker", "go")),
        )
        .await
        .unwrap();
        next_completion(&state, "s-ask").await;
        assert!(offers_ask_human(&provider.last_specs()));
    }

    #[tokio::test]
    async fn background_dispatch_returns_at_once_and_completes_later() {
        let state = crate::serve::ServeState::for_test(false);
        let provider = GatedProvider::new(vec![text_response("child answer")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));

        let started = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some("s-bg"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        assert!(
            started.starts_with("Started background task bg-"),
            "{started}"
        );
        assert!(started.contains("subagent \"reviewer\""), "{started}");
        assert!(
            started
                .ends_with("Its result will be delivered to this conversation when it finishes."),
            "{started}"
        );
        let handle = started_handle(&started);

        provider.wait_entered().await;
        assert!(state.background.drain("s-bg").is_empty());
        assert_eq!(state.background.pending("s-bg").len(), 1);

        provider.gate.notify_one();
        let done = next_completion(&state, "s-bg").await;
        assert_eq!(
            done.kind,
            crate::background::TaskKind::Subagent {
                agent: "reviewer".to_string(),
                handle: handle.clone(),
            }
        );
        match done.outcome {
            crate::background::Outcome::Finished { text, refused } => {
                assert!(text.contains("child answer"), "{text}");
                assert!(refused.is_empty());
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(
            state
                .subagent_cache
                .as_ref()
                .unwrap()
                .get(&handle)
                .is_some(),
            "the background child's history is stored under its handle"
        );
    }

    #[tokio::test]
    async fn background_completion_carries_refused_calls() {
        let state = crate::serve::ServeState::for_test(false);
        let other = spec_named("dangerous");
        state
            .tools
            .register_tool(Box::new(OtherTool(other.clone())))
            .await;
        let provider = ScriptedProvider::new(vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-d".to_string(),
                    name: "dangerous".to_string(),
                    input: json!({"x": 1}),
                }],
                stop_reason: None,
            },
            text_response("could not do it"),
        ]);
        let parent = std::sync::Arc::new(StandingParent::new(None));
        let tool = std::sync::Arc::new(SubagentTool::new(worker_defs(), true));

        crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                provider as std::sync::Arc<dyn crate::provider::Provider>,
                vec![other],
                std::sync::Arc::clone(&parent) as std::sync::Arc<dyn crate::serve::TurnHost>,
                Some("s-bg-refused"),
                0,
            ),
            Tool::execute(&tool, &bg_input("worker", "go")),
        )
        .await
        .unwrap();

        let done = next_completion(&state, "s-bg-refused").await;
        match done.outcome {
            crate::background::Outcome::Finished { refused, .. } => assert_eq!(
                refused,
                vec![crate::background::RefusedCall {
                    name: "dangerous".to_string(),
                    input: json!({"x": 1}),
                }]
            ),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!parent.asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    async fn refused_background(session_id: Option<&str>, depth: u32) -> String {
        let state = crate::serve::ServeState::for_test(false);
        let provider = ScriptedProvider::new(vec![text_response("never")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let err = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                session_id,
                depth,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .expect_err("background must be refused here")
        .to_string();
        assert_eq!(provider.call_count(), 0, "no child may start");
        err
    }

    #[tokio::test]
    async fn background_is_refused_without_a_session() {
        let err = refused_background(None, 0).await;
        assert!(err.contains("main conversation"), "{err}");
    }

    #[tokio::test]
    async fn background_is_refused_at_depth_one() {
        let err = refused_background(Some("s-bg-depth"), 1).await;
        assert!(err.contains("main conversation"), "{err}");
    }

    #[tokio::test]
    async fn background_is_refused_past_the_cap() {
        let state = crate::serve::ServeState::for_test(false);
        for i in 0..4 {
            state
                .background
                .register(
                    "s-bg-cap",
                    crate::background::TaskKind::Subagent {
                        agent: "reviewer".to_string(),
                        handle: format!("h-{i}"),
                    },
                    "held".to_string(),
                )
                .unwrap();
        }
        let provider = ScriptedProvider::new(vec![text_response("never")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let err = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some("s-bg-cap"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .expect_err("a fifth task is over the cap")
        .to_string();
        assert!(err.contains("background_task"), "{err}");
        assert_eq!(provider.call_count(), 0);
        assert_eq!(state.background.pending("s-bg-cap").len(), 4);
    }

    #[tokio::test]
    async fn resuming_a_handle_running_in_background_is_refused() {
        let state = crate::serve::ServeState::for_test(false);
        let provider = GatedProvider::new(vec![text_response("child answer")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let started = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some("s-bg-busy"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        let handle = started_handle(&started);
        provider.wait_entered().await;

        let resume = json!({"resume": handle, "prompt": "more"});
        let resume_provider = ScriptedProvider::new(vec![text_response("resumed")]);
        let err = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&resume_provider)
                    as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            Tool::execute(&tool, &resume),
        )
        .await
        .expect_err("the handle is still running in the background")
        .to_string();
        assert!(err.contains("already in use"), "{err}");

        // Once the child finishes the handle is free again.
        provider.gate.notify_one();
        next_completion(&state, "s-bg-busy").await;
        let resumed = crate::serve::scope_turn_context(
            turn_context(
                std::sync::Arc::clone(&state),
                resume_provider as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
            ),
            Tool::execute(&tool, &resume),
        )
        .await
        .unwrap();
        assert!(resumed.contains("resumed"), "{resumed}");
    }

    #[tokio::test]
    async fn a_background_timeout_completes_as_failed_with_the_handle() {
        let mut state = crate::serve::ServeState::for_test(false);
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .config
            .tools
            .subagent
            .turn_timeout_secs = 1;
        // Never let through: the child's turn outlives its deadline.
        let provider = GatedProvider::new(vec![text_response("never")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let started = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                provider as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some("s-bg-timeout"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        let handle = started_handle(&started);

        let done = next_completion(&state, "s-bg-timeout").await;
        match done.outcome {
            crate::background::Outcome::Failed { message } => {
                assert!(message.contains("timed out"), "{message}");
                assert!(message.contains(&handle), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn has_background(spec: &ToolSpec) -> bool {
        spec.input_schema["properties"].get("background").is_some()
    }

    #[test]
    fn the_nested_spec_never_offers_background() {
        let main = SubagentTool::new(defs(), true);
        assert!(has_background(main.spec()));
        assert!(has_background(&main.live_spec()));
        let off = SubagentTool::new(defs(), false);
        assert!(!has_background(off.spec()));
        assert!(!has_background(&off.live_spec()));

        let parent_visible = vec![main.spec().clone()];
        for subagents in [None, Some(vec!["reviewer".to_string()])] {
            let def = crate::agents::AgentDef {
                name: "lead".to_string(),
                description: "Leads.".to_string(),
                tools: None,
                subagents: subagents.clone(),
                prompt: "You lead.".to_string(),
                profile: None,
            };
            let specs = subagent_tool_specs(&def, &parent_visible, &defs(), 0, 3);
            let nested = specs
                .iter()
                .find(|s| s.name.as_ref() == SUBAGENT_TOOL_NAME)
                .unwrap_or_else(|| panic!("nested subagent offered ({subagents:?})"));
            assert!(
                !has_background(nested),
                "a nested turn is never offered background ({subagents:?})"
            );
        }
    }

    #[tokio::test]
    async fn background_survives_the_parent_turn_ending() {
        let mut state = crate::serve::ServeState::for_test(false);
        let parent = ScriptedProvider::new(vec![
            crate::provider::ChatResponse {
                prompt_usage: None,
                text: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-bg".to_string(),
                    name: SUBAGENT_TOOL_NAME.to_string(),
                    input: bg_input("worker", "go"),
                }],
                stop_reason: None,
            },
            text_response("parent done"),
        ]);
        let child = GatedProvider::new(vec![text_response("child done")]);
        let provider = std::sync::Arc::new(ByPromptProvider {
            child_prompt: "You are a worker.".to_string(),
            child: std::sync::Arc::clone(&child) as std::sync::Arc<dyn crate::provider::Provider>,
            parent: std::sync::Arc::clone(&parent) as std::sync::Arc<dyn crate::provider::Provider>,
        });
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .registry = std::sync::Arc::new(crate::provider::registry::ProviderRegistry::for_test(
            &["anthropic", "stub"],
            provider,
        ));
        state
            .tools
            .register_tool(Box::new(std::sync::Arc::new(SubagentTool::new(
                worker_defs(),
                true,
            ))))
            .await;

        let outcome = crate::serve::run_llm_turn(
            std::sync::Arc::clone(&state),
            "s-bg-parent".to_string(),
            ChatMessage::user("delegate it"),
            std::sync::Arc::new(crate::serve::NullProgress),
            None,
        )
        .await;
        assert_eq!(outcome.text.as_deref(), Some("parent done"));

        child.wait_entered().await;
        assert_eq!(state.background.pending("s-bg-parent").len(), 1);
        child.gate.notify_one();
        let done = next_completion(&state, "s-bg-parent").await;
        match done.outcome {
            crate::background::Outcome::Finished { text, .. } => {
                assert!(text.contains("child done"), "{text}")
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// The request that starts a background child must be able to end
    /// before the child does. An HTTP turn's SSE stream closes only once
    /// every `Sender` is dropped, so a child that kept the parent's
    /// `SseProgress` would hold `/chat`'s response open until it finished.
    #[tokio::test]
    async fn a_background_child_does_not_hold_the_parents_stream_open() {
        let state = crate::serve::ServeState::for_test(false);
        let provider = GatedProvider::new(vec![text_response("child answer")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let parent: std::sync::Arc<dyn crate::serve::TurnHost> =
            std::sync::Arc::new(crate::serve::SseProgress::new(tx, json!(1)));

        crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::clone(&parent),
                Some("s-bg-sse"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        provider.wait_entered().await;
        // The parent turn ends: its context went with the scope above,
        // and this is its host.
        drop(parent);

        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the parent's stream must close while the child still runs");
        assert!(closed.is_none(), "nothing is sent to an ended request");
        assert_eq!(state.background.pending("s-bg-sse").len(), 1);

        provider.gate.notify_one();
        let done = next_completion(&state, "s-bg-sse").await;
        assert!(matches!(
            done.outcome,
            crate::background::Outcome::Finished { .. }
        ));
    }

    /// What a background task keeps by default is the permission row and
    /// nothing else.
    #[test]
    fn the_default_detached_host_keeps_only_the_origin() {
        let parent = StandingParent::new(Some(crate::tools::policy::Approval::AllowAlways));
        let detached = crate::serve::TurnHost::detached(&parent);
        assert_eq!(
            detached.origin(),
            crate::tools::policy::Origin::Acp(crate::tools::policy::SessionMode::Default)
        );
        assert!(detached.acp_client().is_none());
    }

    /// A provider whose `chat` panics, standing in for a panicking tool
    /// or provider inside a background child's turn.
    struct PanickingProvider;

    #[async_trait]
    impl crate::provider::Provider for PanickingProvider {
        fn name(&self) -> &str {
            "panicking"
        }

        async fn chat(
            &self,
            _system: Option<&str>,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
        ) -> anyhow::Result<crate::provider::ChatResponse> {
            panic!("boom");
        }
    }

    /// A panic inside the child still completes its task: as `Failed`,
    /// with the panic's message, the handle released and the session's
    /// registry able to drain to empty. Without it the task would hold a
    /// cap slot and keep a suspended autonomous session waiting forever.
    #[tokio::test]
    async fn a_panicking_background_child_completes_as_failed() {
        let state = crate::serve::ServeState::for_test(false);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let started = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(&state),
                std::sync::Arc::new(PanickingProvider),
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some("s-bg-panic"),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        let handle = started_handle(&started);

        let done = next_completion(&state, "s-bg-panic").await;
        match done.outcome {
            crate::background::Outcome::Failed { message } => {
                assert_eq!(message, "subagent panicked: boom")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        state.background.commit_delivery("s-bg-panic");
        assert!(!state.background.has_pending_or_inbox("s-bg-panic"));
        assert!(
            !tool.busy_handles.lock().unwrap().contains(&handle),
            "the panicked child's handle is released"
        );
    }

    /// Start a background dispatch of `reviewer` whose child never gets
    /// past its first provider call, then cancel it. Returns its handle
    /// and the rendered completion, as delivery renders it.
    async fn cancelled_dispatch(
        state: &std::sync::Arc<crate::serve::ServeState>,
        session: &str,
    ) -> (String, String) {
        let provider = GatedProvider::new(vec![text_response("never")]);
        let tool = std::sync::Arc::new(SubagentTool::new(defs(), true));
        let started = crate::serve::scope_turn_context(
            background_context(
                std::sync::Arc::clone(state),
                std::sync::Arc::clone(&provider) as std::sync::Arc<dyn crate::provider::Provider>,
                Vec::new(),
                std::sync::Arc::new(crate::serve::NullProgress),
                Some(session),
                0,
            ),
            Tool::execute(&tool, &bg_input("reviewer", "go")),
        )
        .await
        .unwrap();
        let handle = started_handle(&started);
        provider.wait_entered().await;
        let id = state.background.pending(session)[0].id.clone();
        state.background.cancel(session, &id).unwrap();
        let done = next_completion(state, session).await;
        let resumable = crate::background::resumable(state.subagent_cache.as_deref(), &done);
        (handle, crate::background::render(&done, resumable))
    }

    /// A fresh background dispatch stores its prompt before it runs, so a
    /// child cancelled (or cut off by a restart) before it finished can
    /// still be resumed, and its completion says so.
    #[tokio::test]
    async fn an_unfinished_background_dispatch_is_resumable_from_its_prompt() {
        let state = crate::serve::ServeState::for_test(false);
        let (handle, text) = cancelled_dispatch(&state, "s-bg-stub").await;
        let stored = state
            .subagent_cache
            .as_ref()
            .unwrap()
            .get(&handle)
            .expect("the stub is stored under the handle");
        assert_eq!(stored.agent, "reviewer");
        assert_eq!(stored.history.len(), 1);
        assert_eq!(stored.history[0].role, crate::provider::Role::User);
        assert!(
            text.contains(&format!("pass handle {handle} as `resume`")),
            "{text}"
        );

        let interrupted = crate::background::Completion {
            task_id: "bg-1".to_string(),
            kind: crate::background::TaskKind::Subagent {
                agent: "reviewer".to_string(),
                handle: handle.clone(),
            },
            outcome: crate::background::Outcome::Interrupted,
            finished_at: chrono::Utc::now(),
        };
        let resumable = crate::background::resumable(state.subagent_cache.as_deref(), &interrupted);
        assert!(
            crate::background::render(&interrupted, resumable)
                .contains(&format!("pass handle {handle} as `resume`"))
        );
    }

    /// With no resume cache nothing can be resumed, so no completion
    /// offers it.
    #[tokio::test]
    async fn without_a_cache_no_completion_offers_resume() {
        let mut state = crate::serve::ServeState::for_test(false);
        std::sync::Arc::get_mut(&mut state)
            .expect("uniquely owned immediately after construction")
            .subagent_cache = None;
        let (_, text) = cancelled_dispatch(&state, "s-bg-nocache").await;
        assert!(text.contains("was cancelled"), "{text}");
        assert!(!text.contains("`resume`"), "{text}");
    }
}
