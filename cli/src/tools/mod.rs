pub mod acp_client;
pub mod ambient_tools;
pub mod builtin_tools;
pub mod client_exec;
pub mod client_tools;
pub mod config_tools;
pub mod policy;
pub mod session_tools;
pub mod skill_tools;
pub mod subagent;
pub mod workspace_tools;

use crate::config::McpServerConfig;
use crate::mcp_client::{self, McpClient, build_tools_for_client};
use crate::provider::{ToolCall, ToolSpec};
pub use agent_client_protocol::schema::v1::ToolKind;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Output of a tool execution.
///
/// Most tools return only text — for those, construct via `String` (impl
/// `From<String>` below). Tools that also need to deliver image bytes to
/// a multimodal model (e.g. `recall_image`, fetching a past image from
/// the cache) populate `images` so the agent runtime can attach them to
/// the user message carrying the tool_result block.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    /// Text content of the tool result.
    pub text: String,
    /// Optional inline image attachments — `(media_type, data_base64)`.
    /// Attached as `ContentPart::Image` parts on the tool_result user
    /// message; only providers that understand image input use them.
    pub images: Vec<(String, String)>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// A tool the agent can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The spec advertised to the LLM.
    fn spec(&self) -> &ToolSpec;

    /// What this tool does, in ACP's vocabulary. Drives both the
    /// `session/update` display and the permission policy, so there is
    /// exactly one classification rather than two that drift apart.
    ///
    /// The default is `Other` — the strictest bucket — so a tool added
    /// without a `kind()` fails safe: it asks (ACP) or is refused
    /// (channels) rather than silently running unguarded.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    /// Execute the tool with the given JSON input. Used by all tools
    /// that return only text — which is most of them.
    async fn execute(&self, input: &serde_json::Value) -> Result<String>;

    /// Execute the tool and return a `ToolOutput` carrying both text
    /// and any image attachments. The default impl wraps `execute`;
    /// override when the tool needs to return image bytes (e.g.
    /// `recall_image`).
    async fn execute_full(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        Ok(ToolOutput::from(self.execute(input).await?))
    }
}

/// A collection of tools with their specs.
///
/// Tools and specs are behind a `RwLock` so that MCP server tool lists can be
/// refreshed at runtime when a `notifications/tools/list_changed` is received.
pub struct ToolSet {
    inner: RwLock<ToolSetInner>,
    /// MCP clients whose `tools_changed` flag is checked before each turn.
    mcp_clients: Vec<Arc<McpClient>>,
}

struct ToolSetInner {
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
}

impl ToolSet {
    pub fn new(tools: Vec<Box<dyn Tool>>, mcp_clients: Vec<Arc<McpClient>>) -> Self {
        let specs = tools.iter().map(|t| t.spec().clone()).collect();
        // `Box` at the public API (every call site already builds one),
        // `Arc` in storage — see `execute` below for why: a tool's
        // execution must be clonable out from under the read guard
        // rather than run while the guard is held.
        let tools: Vec<Arc<dyn Tool>> = tools.into_iter().map(Arc::from).collect();
        Self {
            inner: RwLock::new(ToolSetInner { tools, specs }),
            mcp_clients,
        }
    }

    /// The specs a particular turn should see.
    ///
    /// A tool the caller cannot use is worse than absent: the model
    /// spends a round trip discovering the refusal, and on an ACP
    /// session it may pick the host's `file_read` when it meant the
    /// editor's.
    pub async fn specs_filtered(&self, keep: impl Fn(&str) -> bool) -> Vec<ToolSpec> {
        self.inner
            .read()
            .await
            .specs
            .iter()
            .filter(|s| keep(&s.name))
            .cloned()
            .collect()
    }

    /// Every registered tool's name and kind. Exists so the policy test
    /// can pin the whole classification table in one assertion rather
    /// than constructing each tool by hand.
    pub async fn kinds(&self) -> Vec<(String, ToolKind)> {
        self.inner
            .read()
            .await
            .tools
            .iter()
            .map(|t| (t.spec().name.to_string(), t.kind()))
            .collect()
    }

    /// Execute a tool call. The returned `ToolOutput` carries the
    /// text result plus any image attachments the tool produced; the
    /// caller is responsible for assembling them into a tool_result
    /// user message.
    ///
    /// The text is capped here — see `truncate_output`. Note this is
    /// not *quite* every tool_result the model sees: a call refused by
    /// the permission gate never reaches this function, and its
    /// `ToolOutput` is built directly by the caller
    /// (`src/serve/mod.rs`, `src/agent.rs`). Those strings come from
    /// `policy::refusal_message` and are short by construction, but
    /// anything added on that path is NOT capped by this.
    ///
    /// The read guard is held only long enough to find and clone the
    /// matching `Arc<dyn Tool>` — never across `execute_full` itself.
    /// That matters beyond ordinary lock hygiene: `subagent`
    /// (`crate::tools::subagent::SubagentTool`) runs *inside* this
    /// function and drives a whole nested `TurnLoop`, whose own tool
    /// calls come back through this very function on the same task.
    /// `tokio::sync::RwLock` is task-fair — a reader blocks as soon as a
    /// writer is queued — so holding the guard here across execution
    /// would let a concurrent writer (an MCP `tools/list_changed`
    /// refresh via `refresh_if_needed`, or `mcp_reconnect`) queue behind
    /// this call and then block the nested re-entrant read behind
    /// *that* writer, with nothing left to release either side: the
    /// whole `ToolSet`, on every transport, stops answering until
    /// restart. Cloning the `Arc` and dropping the guard before
    /// `execute_full` removes the possibility outright, for `subagent`
    /// and for every other tool alike — including `mcp_reconnect`
    /// itself, which no longer deadlocks on its own write lock while
    /// its own call's read guard is still held (#201).
    pub async fn execute(&self, call: &ToolCall) -> ToolOutput {
        let tool = {
            let inner = self.inner.read().await;
            inner
                .tools
                .iter()
                .find(|t| t.spec().name == call.name)
                .cloned()
        };
        let Some(tool) = tool else {
            return ToolOutput::from(format!("Unknown tool: {}", call.name));
        };
        let mut output = match tool.execute_full(&call.input).await {
            Ok(output) => output,
            Err(e) => ToolOutput::from(format!("Error: {e:#}")),
        };
        output.text = truncate_output(&output.text);
        output
    }

    /// Check all MCP clients for `tools_changed` flags and refresh their
    /// tools if needed.  Should be called before each LLM turn.
    pub async fn refresh_if_needed(&self) {
        for client in &self.mcp_clients {
            if !client.take_tools_changed() {
                continue;
            }
            if let Err(e) = self.refresh_client_tools(client).await {
                warn!("MCP '{}': failed to refresh tools: {e:#}", client.name());
            }
        }
    }

    /// Re-list a client's tools and swap them into the ToolSet.
    async fn refresh_client_tools(&self, client: &Arc<McpClient>) -> Result<()> {
        info!("MCP '{}': refreshing tool list", client.name());
        let remote_tools = client.list_tools().await?;
        let new_tools = build_tools_for_client(client, remote_tools);
        let prefix = format!("mcp__{}__", client.name());

        let mut inner = self.inner.write().await;
        inner.tools.retain(|t| !t.spec().name.starts_with(&prefix));
        inner.specs.retain(|s| !s.name.starts_with(&prefix));
        for tool in new_tools {
            inner.specs.push(tool.spec().clone());
            inner.tools.push(Arc::from(tool));
        }
        info!(
            "MCP '{}': tool list refreshed ({} total tools)",
            client.name(),
            inner.tools.len()
        );
        Ok(())
    }

    /// The connected client for the MCP server called `name`, if it connected.
    pub fn mcp_client(&self, name: &str) -> Option<Arc<McpClient>> {
        self.mcp_clients.iter().find(|c| c.name() == name).cloned()
    }

    /// Names of configured MCP servers (for tool discovery / error messages).
    pub fn mcp_server_names(&self) -> Vec<String> {
        self.mcp_clients
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    }

    /// Reconnect one MCP server by name and refresh its tool list.
    /// Returns a human-readable status summary.
    pub async fn reconnect_mcp_server(&self, name: &str) -> Result<String> {
        let client = self
            .mcp_clients
            .iter()
            .find(|c| c.name() == name)
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server: {name}"))?;

        client.reconnect().await?;
        self.refresh_client_tools(client).await?;
        Ok(format!(
            "Reconnected MCP server '{name}' and refreshed its tools."
        ))
    }

    /// Register an additional tool after construction.
    pub async fn register_tool(&self, tool: Box<dyn Tool>) {
        let mut inner = self.inner.write().await;
        inner.specs.push(tool.spec().clone());
        inner.tools.push(Arc::from(tool));
    }

    /// Swap, in place, the spec discovery hands the model for a tool
    /// that is already registered.
    ///
    /// This exists because `Tool::spec` returns a borrow: a tool whose
    /// description depends on state it can change has no way to publish
    /// that state through its own `spec()` — there is nowhere for the
    /// borrow to come from that outlives the lock guarding the state.
    /// So the spec the model is offered is kept here instead, as the
    /// authoritative copy, and whatever changes the state updates this
    /// copy too: `subagent`'s definition list is the first such state
    /// (#265), swapped with `SubagentTool::set_agents` and rendered by
    /// `SubagentTool::live_spec`.
    ///
    /// Replace only. An unregistered `name` is a `warn!` and nothing
    /// else, on purpose: pushing the spec anyway would leave the set
    /// offering a name `execute` cannot dispatch, which is worse than
    /// not offering it at all — the model would call it, spend a round
    /// trip, and get `Unknown tool` back for something it was just told
    /// exists. Adding a tool is `register_tool`, which carries the tool
    /// itself.
    ///
    /// Callable from inside `Tool::execute`, which is how the config
    /// tools will reach it: `execute` clones the `Arc<dyn Tool>` it
    /// matched and drops its read guard before calling into that tool,
    /// so this write does not queue behind a guard the caller itself
    /// still holds.
    pub async fn replace_spec(&self, name: &str, spec: ToolSpec) {
        let mut inner = self.inner.write().await;
        match inner.specs.iter_mut().find(|s| s.name == name) {
            Some(existing) => *existing = spec,
            None => warn!("replace_spec: no registered tool named '{name}'"),
        }
    }
}

/// Build the default tool set backed by a sapphire-framework WorkspaceState.
///
/// `tavily_api_key`: if provided, the `web_search` tool is included.
/// `mcp_servers`: external MCP servers whose tools are registered with the
/// naming convention `mcp__<name>__<tool_name>`.
/// `day_boundary_hour`: what `current_time` reports as the agent's
/// logical day.
pub async fn default_tool_set(
    state: Arc<sapphire_framework::workspace::WorkspaceState>,
    tavily_api_key: Option<String>,
    mcp_servers: &[McpServerConfig],
    day_boundary_hour: u8,
) -> Arc<ToolSet> {
    use builtin_tools::*;
    use client_tools::*;
    use session_tools::CurrentTimeTool;
    use workspace_tools::*;

    let workspace_root = state.workspace.root.clone();

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(CurrentTimeTool::new(day_boundary_hour)),
        Box::new(MemoryAddTool::new(Arc::clone(&state))),
        Box::new(MemoryReadTool::new(Arc::clone(&state))),
        Box::new(MemoryAppendTool::new(Arc::clone(&state))),
        Box::new(MemoryUpdateTool::new(Arc::clone(&state))),
        Box::new(MemoryRemoveTool::new(Arc::clone(&state))),
        Box::new(WorkspaceSearchTool::new(Arc::clone(&state))),
        Box::new(WorkspaceSyncTool::new(Arc::clone(&state))),
        Box::new(FileReadTool::new(Arc::clone(&state))),
        Box::new(FileWriteTool::new(Arc::clone(&state))),
        Box::new(FileAppendTool::new(Arc::clone(&state))),
        Box::new(FileDeleteTool::new(Arc::clone(&state))),
        Box::new(DirListTool::new(Arc::clone(&state))),
        Box::new(DirWalkTool::new(Arc::clone(&state))),
        Box::new(ShellTool::new(workspace_root.clone())),
        Box::new(ClientShellStart::new()),
        Box::new(ClientShellOutput::new()),
        Box::new(ClientShellKill::new()),
        Box::new(WeatherTool::new()),
    ];

    if let Some(key) = tavily_api_key {
        tools.push(Box::new(WebSearchTool::new(key)));
    }

    // External MCP server tools
    let mut mcp_clients = Vec::new();
    if !mcp_servers.is_empty() {
        let workspace_root_str = workspace_root.to_string_lossy();
        let (mcp_tools, clients) =
            mcp_client::create_mcp_tools(mcp_servers, &workspace_root_str).await;
        tools.extend(mcp_tools);
        mcp_clients = clients;
    }

    let tool_set = Arc::new(ToolSet::new(tools, mcp_clients));

    // Register the reconnect tool only if at least one MCP server is configured.
    if !mcp_servers.is_empty() {
        let reconnect = Box::new(builtin_tools::McpReconnectTool::new(Arc::downgrade(
            &tool_set,
        )));
        tool_set.register_tool(reconnect).await;
    }

    tool_set
}

/// The cap shared by `truncate_output` (tool *results*) and
/// `AcpSessionStore::store_part` (tool *inputs* — see Fix 4/#194): both
/// bound what a single tool call can put into memory, the model's
/// input, and an indexed on-disk file. Kept as one constant so the two
/// call sites cannot drift apart.
pub(crate) const OUTPUT_CAP_BYTES: usize = 50_000;

/// Cap a tool result at 50 000 bytes, keeping head + tail.
///
/// Applied at `ToolSet::execute` rather than inside each tool, because
/// that is the one place every builtin and every MCP tool's output
/// passes through — per-tool truncation would be forgotten by the next
/// tool someone adds.
///
/// The cap is what bounds a tool result's cost in all three places it
/// lands: the in-memory history, the model's input, and (for ACP
/// sessions) the tool-result cache on disk.
///
/// Head and tail both survive because they carry different things: a
/// file's shape is at the top, and a failing command's error is at the
/// bottom.
///
/// The head, tail and marker budgets add up to `MAX` rather than
/// overshooting it, so truncating an already-truncated result is a
/// no-op. The previous constants (20 000 + 30 000, plus the marker on
/// top) exceeded the cap, which meant a second pass cut again and
/// nested the markers — harmless only for as long as truncation
/// happened in exactly one place, which is what this change ends.
///
/// The cap and the marker are both byte counts, not character counts —
/// the mechanism has to be byte-based to actually bound memory and disk
/// usage. For CJK text, where a character is 3 bytes, the effective
/// cap is roughly a third of the number that appears here.
pub(crate) fn truncate_output(s: &str) -> String {
    const MAX: usize = OUTPUT_CAP_BYTES;
    // Room for `\n\n[... 1234567 bytes truncated ...]\n\n`, generously.
    const MARKER_BUDGET: usize = 200;
    const HEAD: usize = 19_920;
    const TAIL: usize = MAX - MARKER_BUDGET - HEAD;

    if s.len() <= MAX {
        return s.to_string();
    }
    let head_end = s.floor_char_boundary(HEAD);
    let tail_start = s.floor_char_boundary(s.len() - TAIL);
    format!(
        "{}\n\n[... {} bytes truncated ...]\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_framework::workspace::{AppContext, Workspace, WorkspaceState};

    static TEST_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

    fn test_workspace() -> Arc<WorkspaceState> {
        // AppContext panics on any cache_dir() access until set_cache_dir has
        // been called once; Workspace::from_root reads it to compute the
        // workspace's cache path. set_cache_dir is "first writer wins" and
        // silently ignores later calls, so it is safe to call on every
        // invocation of this helper (matches sapphire-framework's own
        // workspace_state.rs test pattern).
        TEST_CTX.set_cache_dir(std::env::temp_dir().join("sapphire-agent-tools-test-cache"));
        // Leaked on purpose: this is a test binary and the OS reclaims
        // the directory when it exits.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::fs::create_dir_all(dir.path().join(".sapphire-agent")).unwrap();
        let ws = Workspace::from_root(&TEST_CTX, dir.path()).unwrap();
        Arc::new(WorkspaceState::open(ws).unwrap())
    }

    /// Every tool declares what it does. A tool added without a `kind()`
    /// lands in `Other` — the strictest bucket — so this table failing
    /// on a newly added tool is the intended prompt to classify it.
    #[tokio::test]
    async fn every_tool_declares_its_kind() {
        let tools = default_tool_set(
            test_workspace(),
            Some("test-tavily-key".to_string()),
            &[],
            4,
        )
        .await;

        let mut got = tools.kinds().await;
        got.sort_by(|a, b| a.0.cmp(&b.0));
        let got_refs: Vec<(&str, ToolKind)> = got.iter().map(|(n, k)| (n.as_str(), *k)).collect();

        let want: Vec<(&str, ToolKind)> = vec![
            ("current_time", ToolKind::Read),
            ("dir_list", ToolKind::Search),
            ("dir_walk", ToolKind::Search),
            ("file_append", ToolKind::Edit),
            ("file_delete", ToolKind::Delete),
            ("file_read", ToolKind::Read),
            ("file_write", ToolKind::Edit),
            ("memory_add", ToolKind::Edit),
            ("memory_append", ToolKind::Edit),
            ("memory_read", ToolKind::Read),
            ("memory_remove", ToolKind::Delete),
            ("memory_update", ToolKind::Edit),
            ("shell", ToolKind::Execute),
            ("shell_kill", ToolKind::Execute),
            ("shell_output", ToolKind::Read),
            ("shell_start", ToolKind::Execute),
            ("weather", ToolKind::Fetch),
            ("web_search", ToolKind::Fetch),
            ("workspace_search", ToolKind::Search),
            ("workspace_sync", ToolKind::Other),
        ];

        assert_eq!(got_refs, want);
    }

    /// `mcp_reconnect` is registered only when an MCP server is
    /// configured, so `every_tool_declares_its_kind` never sees it —
    /// and it is the one tool that deliberately relies on the *default*
    /// `Other` rather than declaring a kind. That makes it the case a
    /// careless `kind()` could silently soften with nothing noticing.
    #[test]
    fn mcp_reconnect_stays_in_the_strict_bucket() {
        let tool = builtin_tools::McpReconnectTool::new(std::sync::Weak::new());
        assert_eq!(tool.spec().name, "mcp_reconnect");
        assert_eq!(tool.kind(), ToolKind::Other);
    }

    /// A tool that does not override `kind()` must land in the strictest
    /// bucket, so forgetting to classify one fails safe.
    #[test]
    fn the_default_kind_is_other() {
        struct Bare(ToolSpec);
        #[async_trait]
        impl Tool for Bare {
            fn spec(&self) -> &ToolSpec {
                &self.0
            }
            async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
                Ok(String::new())
            }
        }
        let bare = Bare(ToolSpec {
            name: "bare".into(),
            description: String::new().into(),
            input_schema: serde_json::json!({}),
        });
        assert_eq!(bare.kind(), ToolKind::Other);
    }

    /// A minimal named stub for tests that only care about a tool's
    /// name being present in a spec list — modeled on `Bare` above, but
    /// keeping the name so `specs_filtered`'s predicate has something
    /// to match against.
    struct NamedStub(ToolSpec);

    impl NamedStub {
        fn new(name: &str) -> Self {
            Self(ToolSpec {
                name: name.to_string().into(),
                description: String::new().into(),
                input_schema: serde_json::json!({}),
            })
        }
    }

    #[async_trait]
    impl Tool for NamedStub {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
    }

    impl ToolSet {
        /// A `ToolSet` with nothing registered, for tests that only
        /// want to exercise `register_tool` / `specs_filtered` without
        /// building a real workspace.
        fn new_empty_for_test() -> Self {
            ToolSet::new(Vec::new(), Vec::new())
        }
    }

    #[tokio::test]
    async fn specs_filtered_keeps_only_what_the_predicate_allows() {
        let tools = ToolSet::new_empty_for_test();
        tools
            .register_tool(Box::new(NamedStub::new("keep_me")))
            .await;
        tools
            .register_tool(Box::new(NamedStub::new("drop_me")))
            .await;

        let names: Vec<String> = tools
            .specs_filtered(|n| n == "keep_me")
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(names, vec!["keep_me".to_string()]);
    }

    /// `replace_spec` swaps the spec the model is offered — same
    /// count, new description — rather than appending a second entry
    /// for a name that is already registered.
    #[tokio::test]
    async fn replace_spec_swaps_what_the_model_is_offered() {
        let set = ToolSet::new(
            vec![Box::new(crate::tools::subagent::SubagentTool::new(Vec::new())) as Box<dyn Tool>],
            Vec::new(),
        );
        let mut swapped = set.specs_filtered(|_| true).await[0].clone();
        swapped.description = "swapped".into();
        set.replace_spec("subagent", swapped).await;
        let after = set.specs_filtered(|_| true).await;
        assert_eq!(after.len(), 1, "replace, not append");
        assert_eq!(after[0].description, "swapped");
    }

    /// Naming something unregistered must not *add* it: a spec without
    /// a tool behind it is a call the model can make and cannot
    /// complete.
    #[tokio::test]
    async fn replace_spec_does_not_add_an_unknown_name() {
        let set = ToolSet::new(Vec::new(), Vec::new());
        set.replace_spec(
            "nope",
            crate::provider::ToolSpec {
                name: "nope".into(),
                description: "x".into(),
                input_schema: serde_json::json!({}),
            },
        )
        .await;
        assert!(set.specs_filtered(|_| true).await.is_empty());
    }

    /// Regression test for the Critical deadlock fixed in `execute`'s doc
    /// above: a queued writer must never be blocked behind a tool that is
    /// still executing. Uses a tool that blocks inside `execute` until
    /// released, signalling once it has actually entered so the test
    /// never races the spawn against the tool starting to run — by the
    /// time that signal fires, the read guard that located the tool is
    /// guaranteed to have already been dropped.
    ///
    /// Sanity-checked by temporarily re-widening `execute`'s read guard to
    /// span `execute_full` again: with that regression reintroduced, this
    /// test times out and fails, as expected of a test with teeth.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_writer_is_not_blocked_by_an_executing_tool() {
        use std::time::Duration;
        use tokio::sync::Notify;

        /// A tool whose `execute` blocks until `release` is notified.
        struct BlockingTool {
            spec: ToolSpec,
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }

        #[async_trait]
        impl Tool for BlockingTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok("blocking tool result".to_string())
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let tools = Arc::new(ToolSet::new_empty_for_test());
        tools
            .register_tool(Box::new(BlockingTool {
                spec: ToolSpec {
                    name: "blocking".to_string().into(),
                    description: String::new().into(),
                    input_schema: serde_json::json!({}),
                },
                entered: entered.clone(),
                release: release.clone(),
            }))
            .await;

        let call = ToolCall {
            id: "1".to_string(),
            name: "blocking".to_string(),
            input: serde_json::json!({}),
        };

        let exec_tools = tools.clone();
        let handle = tokio::spawn(async move { exec_tools.execute(&call).await });

        // Wait until the tool is actually inside `execute_full`, so the
        // read guard that located it is guaranteed to have been dropped.
        entered.notified().await;

        // A writer queued behind that dropped guard must not be blocked
        // by the tool's still-running execution.
        tokio::time::timeout(
            Duration::from_secs(2),
            tools.register_tool(Box::new(NamedStub::new("late"))),
        )
        .await
        .expect(
            "a queued writer must not block behind an executing tool — ToolSet::execute is holding its read guard across execution again",
        );

        release.notify_one();

        let output = handle.await.expect("execute task panicked");
        assert_eq!(output.text, "blocking tool result");
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn short_output_is_returned_unchanged() {
        let s = "a short result";
        assert_eq!(truncate_output(s), s);
    }

    /// The cap exists so one `file_read` of a large file cannot put the
    /// whole file into the in-memory history, the model's input, and the
    /// cache all at once.
    #[test]
    fn long_output_is_cut_to_the_cap_with_a_marker() {
        let s = "x".repeat(120_000);
        let out = truncate_output(&s);
        assert!(
            out.len() < s.len(),
            "a 120k result must not come back whole"
        );
        assert!(out.contains("bytes truncated"), "got {}", &out[..80]);
    }

    /// Head and tail are both kept: the head is where a file's shape is,
    /// the tail is where a failing command's error is.
    #[test]
    fn both_ends_survive_truncation() {
        let s = format!(
            "{}{}{}",
            "H".repeat(30_000),
            "M".repeat(60_000),
            "T".repeat(30_000)
        );
        let out = truncate_output(&s);
        assert!(out.starts_with('H'), "the head is kept");
        assert!(out.ends_with('T'), "the tail is kept");
        assert!(
            !out.contains(&"M".repeat(1000)),
            "the middle is what gets dropped"
        );
    }

    /// Cutting at a byte index inside a multi-byte character would
    /// panic. `floor_char_boundary` is what prevents it, so a result
    /// that is entirely multi-byte is the case worth pinning.
    ///
    /// A leading ASCII byte is prepended so the fixed-width 3-byte `日`
    /// characters no longer line up with HEAD (19 920) and the tail
    /// start — both multiples of 3 for a bare repeated character, so
    /// naive byte indexing would land on a boundary anyway and the test
    /// would pass for the wrong reason.
    #[test]
    fn a_multibyte_result_is_cut_on_a_character_boundary() {
        let s = format!("x{}", "日".repeat(60_000)); // well past the cap
        let out = truncate_output(&s);
        assert!(out.contains("bytes truncated"));
        assert!(out.starts_with('x') && out.ends_with('日'));
    }

    /// The cap has to be a cap: a result that has already been cut must
    /// come back unchanged rather than picking up a second marker.
    ///
    /// The old constants did not satisfy this — head 20 000 + tail
    /// 30 000 + the marker itself exceeds 50 000, so a second pass cut
    /// again and nested the markers. That only stayed invisible because
    /// truncation happened in exactly one place.
    #[test]
    fn truncating_an_already_truncated_result_changes_nothing() {
        let once = truncate_output(&"x".repeat(200_000));
        assert!(
            once.len() <= 50_000,
            "the cap is a cap: {} bytes",
            once.len()
        );
        assert_eq!(truncate_output(&once), once);
        assert_eq!(once.matches("bytes truncated").count(), 1);
    }

    /// A stub tool that always returns a huge result, standing in for
    /// e.g. `file_read` on a large file. Modeled on `RiskyTool` in
    /// `src/serve/mod.rs`.
    struct HugeOutputTool {
        spec: ToolSpec,
    }

    impl HugeOutputTool {
        fn new() -> Self {
            Self {
                spec: ToolSpec {
                    name: "huge_output".into(),
                    description: "Always returns a huge result.".into(),
                    input_schema: serde_json::json!({ "type": "object", "properties": {} }),
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl Tool for HugeOutputTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
            Ok("y".repeat(120_000))
        }
    }

    /// The regression this suite exists to catch: every test above calls
    /// `truncate_output` directly, so deleting the call site at
    /// `ToolSet::execute` (this function, not the helper) would leave
    /// the whole module green while silently reverting the cap on the
    /// one path every tool result actually travels through.
    #[tokio::test]
    async fn tool_set_execute_applies_the_cap() {
        let tool_set = ToolSet::new(vec![Box::new(HugeOutputTool::new())], Vec::new());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "huge_output".to_string(),
            input: serde_json::json!({}),
        };

        let output = tool_set.execute(&call).await;

        assert!(
            output.text.len() <= 50_000,
            "ToolSet::execute must cap the output itself, not rely on the \
             tool to do it: got {} bytes",
            output.text.len()
        );
        assert_eq!(
            output.text.matches("bytes truncated").count(),
            1,
            "exactly one truncation marker: {}",
            &output.text[..200.min(output.text.len())]
        );
    }
}
