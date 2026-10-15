//! Built-in MCP client for connecting to external MCP servers.
//!
//! Each configured MCP server's tools are registered in the agent's `ToolSet`
//! using the naming convention `mcp__<server_name>__<tool_name>`.
//!
//! The protocol itself is `rmcp`'s: its Streamable HTTP and child-process
//! transports, its lifecycle negotiation, and its multi-round-trip handling
//! for server-initiated input. What lives here is the agent's side of it —
//! which callbacks a server gets answered ([`AgentClientHandler`]), when a
//! dead connection is replaced, and how a remote tool becomes a local
//! [`Tool`].

use crate::config::{McpServerConfig, McpTransportConfig, McpTrust};
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ElicitRequestParams,
    ElicitResult, ElicitationAction, ElicitationCapability, ErrorData, Implementation,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    RootsCapabilities, ServerNotification, SubscriptionFilter,
};
#[allow(deprecated)] // Roots are deprecated by SEP-2577, but servers still ask.
use rmcp::model::{ListRootsResult, Root};
use rmcp::service::{
    ClientInitializeError, ClientLifecycleMode, ClientServiceExt, NotificationContext,
    RequestContext, RoleClient, RunningService,
};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ServiceError};
use serde_json::Value;
use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// The handle [`McpClient::listen`] returns: one `subscriptions/listen`
/// stream. Re-exported so callers need not name `rmcp` for it.
#[allow(unused_imports)] // For the timer integration, which listens.
pub use rmcp::service::{Subscription, SubscriptionEnd};

// ---------------------------------------------------------------------------
// Remote tool metadata
// ---------------------------------------------------------------------------

/// A tool specification retrieved from a remote MCP server.
pub struct RemoteToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// Client handler — what the agent answers when a server asks
// ---------------------------------------------------------------------------

/// The agent's answers to server-initiated requests and notifications.
///
/// On a legacy (`initialize`) server these arrive as JSON-RPC requests over
/// the connection; on a 2026-07-28 server they arrive as `input_required`
/// rounds of the call that needed them, which `rmcp` drives through these
/// same methods. Either way the answers are the ones the hand-written client
/// gave.
#[derive(Clone)]
struct AgentClientHandler {
    name: String,
    workspace_root: String,
    /// Set when the server reports its tool list changed. Shared with
    /// [`McpClient`], which outlives any one connection.
    tools_changed: Arc<AtomicBool>,
}

impl ClientHandler for AgentClientHandler {
    /// The workspace is the one root the agent works in.
    #[allow(deprecated)] // Roots are deprecated by SEP-2577, but still asked for.
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Ok(ListRootsResult::new(vec![
            Root::new(format!("file://{}", self.workspace_root)).with_name("workspace"),
        ]))
    }

    /// Accept a form elicitation unasked, echoing its message as the content.
    ///
    /// There is no person on the other end of most turns to ask, and this is
    /// what the agent has always answered. A URL elicitation is declined
    /// instead: accepting one asserts that someone opened the page.
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Ok(match request {
            ElicitRequestParams::FormElicitationParams { message, .. } => {
                ElicitResult::new(ElicitationAction::Accept).with_content(Value::String(message))
            }
            _ => ElicitResult::new(ElicitationAction::Decline),
        })
    }

    // `sampling/createMessage` keeps rmcp's default: method-not-found, and
    // no `sampling` capability is declared.

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP '{}': tool list changed, will refresh", self.name);
        self.tools_changed.store(true, Ordering::Relaxed);
    }

    #[allow(deprecated)] // Roots, as above.
    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(RootsCapabilities::default());
        capabilities.elicitation = Some(ElicitationCapability::new());
        // The version offered to `initialize`. `server/discover` negotiates
        // from `ClientLifecycleMode`'s own list and does not read this.
        ClientConfig::new(
            capabilities,
            Implementation::new("sapphire-agent", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }
}

// ---------------------------------------------------------------------------
// Connection — one negotiated session with a server
// ---------------------------------------------------------------------------

type Service = RunningService<RoleClient, AgentClientHandler>;

/// One live connection, and the background task watching it for tool-list
/// changes. Replaced whole by [`McpClient::reconnect`].
struct Connection {
    service: Service,
    /// The `subscriptions/listen` task that stands in for the unsolicited
    /// `tools/list_changed` notification on a 2026-07-28 server.
    watcher: Option<JoinHandle<()>>,
}

impl Connection {
    /// Whether the service loop has ended — the child exited, the transport
    /// worker gave up, or the connection was closed on purpose.
    fn is_closed(&self) -> bool {
        self.service.is_closed() || self.service.peer().is_transport_closed()
    }

    /// Stop the service without waiting for it, for a connection that may
    /// still be shared with calls in flight. Those calls then fail with
    /// `TransportClosed`, which sends them through the reconnect path.
    fn close(&self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
        self.service.cancellation_token().cancel();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// MCP Client
// ---------------------------------------------------------------------------

/// Client for a single external MCP server.
///
/// Construction does not touch the server; [`Self::connect`] does, and every
/// call after it reconnects on its own when it finds the connection gone.
pub struct McpClient {
    name: String,
    config: McpServerConfig,
    workspace_root: String,
    connection: RwLock<Option<Arc<Connection>>>,
    /// Set to `true` when the server reports `tools/list_changed`.
    tools_changed: Arc<AtomicBool>,
}

impl McpClient {
    /// Create a client for `config`. Nothing is spawned or contacted yet.
    pub fn new(config: &McpServerConfig, workspace_root: &str) -> Self {
        Self {
            name: config.name.clone(),
            config: config.clone(),
            workspace_root: workspace_root.to_string(),
            connection: RwLock::new(None),
            tools_changed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The server name (used as the tool namespace prefix).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How far the operator declared this server's tools to be trusted.
    pub fn trust(&self) -> McpTrust {
        self.config.trust
    }

    /// Check and clear the `tools_changed` flag.
    /// Returns `true` if the tool list has changed since the last check.
    pub fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::Relaxed)
    }

    /// Establish the connection (spawn the process or reach the endpoint and
    /// negotiate the protocol), replacing any existing one.
    pub async fn connect(&self) -> Result<()> {
        let mut slot = self.connection.write().await;
        if let Some(old) = slot.take() {
            old.close();
        }
        *slot = Some(self.open().await?);
        Ok(())
    }

    /// Tear down the existing connection and establish a fresh one.
    ///
    /// On failure the old connection is already gone; the caller may retry.
    pub async fn reconnect(&self) -> Result<()> {
        info!("MCP '{}': reconnecting", self.name);
        self.tools_changed.store(false, Ordering::Relaxed);
        self.connect().await
    }

    /// Close the connection. A later call reconnects.
    #[allow(dead_code)]
    pub async fn shutdown(&self) -> Result<()> {
        if let Some(old) = self.connection.write().await.take() {
            old.close();
        }
        Ok(())
    }

    /// List tools available on the remote MCP server.
    pub async fn list_tools(&self) -> Result<Vec<RemoteToolSpec>> {
        let tools = self
            .run(
                "tools/list",
                |c| async move { c.service.list_all_tools().await },
            )
            .await?;
        let specs: Vec<RemoteToolSpec> = tools
            .into_iter()
            .map(|t| RemoteToolSpec {
                name: t.name.into_owned(),
                description: t.description.map(Cow::into_owned).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect();

        info!("MCP '{}': found {} tools", self.name, specs.len());
        Ok(specs)
    }

    /// Call a tool on the remote MCP server.
    ///
    /// The result is returned whole — `content`, `structuredContent` and
    /// `isError` alike; [`tool_result_text`] flattens it the way the LLM
    /// sees it. Input the server asks for along the way is answered by
    /// [`AgentClientHandler`].
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<CallToolResult> {
        let arguments = match arguments {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => anyhow::bail!("MCP tool arguments must be an object, got: {other}"),
        };
        let mut params = CallToolRequestParams::new(name.to_string());
        params.arguments = arguments;
        self.run("tools/call", |c| {
            let params = params.clone();
            async move { c.service.call_tool(params).await }
        })
        .await
    }

    /// Read one resource from the remote MCP server.
    #[allow(dead_code)] // For the timer integration, which reads resources.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        let params = ReadResourceRequestParams::new(uri);
        self.run("resources/read", |c| {
            let params = params.clone();
            async move { c.service.read_resource(params).await }
        })
        .await
    }

    /// Open a `subscriptions/listen` stream with `filter` and wait for the
    /// server to acknowledge it.
    ///
    /// The subscription is bound to the connection it was opened on: when
    /// that connection is replaced — by [`Self::reconnect`], or by a call
    /// recovering from a dead transport — it ends with
    /// [`SubscriptionEnd::Abrupt`], and the caller listens again. Only a
    /// 2026-07-28 server serves this; a legacy one answers method-not-found.
    #[allow(dead_code)] // For the timer integration, which listens.
    pub async fn listen(&self, filter: SubscriptionFilter) -> Result<Subscription> {
        self.run("subscriptions/listen", |c| {
            let filter = filter.clone();
            async move { c.service.peer().listen(filter).await }
        })
        .await
    }

    /// The live connection, establishing one if there is none or the last
    /// one has died.
    async fn connection(&self) -> Result<Arc<Connection>> {
        if let Some(c) = self.connection.read().await.as_ref()
            && !c.is_closed()
        {
            return Ok(Arc::clone(c));
        }
        self.replace(None).await
    }

    /// Replace the connection `stale` with a fresh one — unless another
    /// caller already has, in which case theirs is returned. `None` means
    /// "whatever is there, if it is dead or missing".
    async fn replace(&self, stale: Option<&Arc<Connection>>) -> Result<Arc<Connection>> {
        let mut slot = self.connection.write().await;
        if let Some(current) = slot.as_ref() {
            let superseded = stale.is_some_and(|s| !Arc::ptr_eq(s, current));
            if (superseded || stale.is_none()) && !current.is_closed() {
                return Ok(Arc::clone(current));
            }
            current.close();
        }
        *slot = None;
        let fresh = self.open().await.with_context(|| {
            format!("MCP '{}': failed to re-establish the connection", self.name)
        })?;
        *slot = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Run `op` against the connection, replacing the connection once if
    /// the transport turns out to be closed.
    ///
    /// What "closed" covers is narrower than it was in the hand-written
    /// client, because `rmcp` already absorbs the common case: a Streamable
    /// HTTP server that forgets an idle session answers `404`, and `rmcp`'s
    /// transport re-runs the handshake and replays the request itself
    /// (`reinit_on_expired_session`, on by default). A 2026-07-28 server has
    /// no session to forget at all. What is left is the connection's service
    /// loop ending — a stdio server that exited, a transport worker that
    /// gave up — after which every request fails with `TransportClosed`
    /// until something replaces it. A plain send failure (the HTTP endpoint
    /// refusing a connection) is *not* retried here: the worker is still
    /// alive and the next call goes through it again.
    ///
    /// The retry is deliberately once. A fresh connection failing the same
    /// way is not staleness, and repeating the handshake against a server
    /// that is down only buries the real error.
    async fn run<T, F, Fut>(&self, method: &str, op: F) -> Result<T>
    where
        F: Fn(Arc<Connection>) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        let conn = self.connection().await?;
        let result = match op(Arc::clone(&conn)).await {
            Err(ServiceError::TransportClosed) => {
                info!(
                    "MCP '{}': {method} found the connection closed; re-establishing it",
                    self.name
                );
                let conn = self.replace(Some(&conn)).await?;
                op(conn).await
            }
            other => other,
        };
        result.map_err(|e| {
            anyhow!(
                "MCP '{}': {method} failed: {}",
                self.name,
                describe_service_error(&e)
            )
        })
    }

    /// Build the transport, negotiate the protocol, and start watching for
    /// tool-list changes.
    async fn open(&self) -> Result<Arc<Connection>> {
        // Prefer the stateless 2026-07-28 lifecycle, falling back to the
        // `initialize` handshake. rmcp's `Auto` mode falls back on its own
        // for a server that rejects `server/discover` (one that ignores it
        // costs a ten-second probe first). It does not for a server that
        // answers `server/discover` but lists only versions that still have
        // `initialize` — an rmcp server capped below 2026-07-28 — so that
        // case gets a second handshake, on a fresh transport, here.
        let auto = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::LATEST],
            legacy_version: None,
        };
        let service = match self.handshake(auto).await {
            Err(e) if offers_only_legacy(&e) => {
                info!(
                    "MCP '{}': server offers only legacy protocol versions; using initialize",
                    self.name
                );
                self.handshake(ClientLifecycleMode::Initialize).await
            }
            other => other,
        };
        let service = service.map_err(|e| match e.downcast_ref::<ClientInitializeError>() {
            Some(init) => anyhow!(
                "MCP '{}': handshake failed: {}",
                self.name,
                describe_init_error(init)
            ),
            None => e,
        })?;

        let peer_info = service.peer().peer_info();
        info!(
            "MCP '{}': connected (server: {}, protocol {})",
            self.name,
            peer_info
                .as_ref()
                .and_then(|i| i.server_info.as_ref())
                .map(|s| s.name.as_str())
                .unwrap_or("unknown"),
            peer_info
                .as_ref()
                .map(|i| i.protocol_version.to_string())
                .unwrap_or_default(),
        );

        let watcher = self.watch_tool_list(&service);
        Ok(Arc::new(Connection { service, watcher }))
    }

    /// Build a fresh transport and run one handshake over it.
    async fn handshake(&self, lifecycle: ClientLifecycleMode) -> Result<Service> {
        let handler = AgentClientHandler {
            name: self.name.clone(),
            workspace_root: self.workspace_root.clone(),
            tools_changed: Arc::clone(&self.tools_changed),
        };
        let service = match &self.config.transport {
            McpTransportConfig::Http { url, api_key } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                if let Some(key) = api_key {
                    // rmcp adds the `Bearer ` prefix itself, and sends it on
                    // every POST and GET — replies to server requests too.
                    config = config.auth_header(key.clone());
                }
                let transport = StreamableHttpClientTransport::from_config(config);
                handler.serve_with_lifecycle(transport, lifecycle).await?
            }
            McpTransportConfig::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args).envs(env);
                let (transport, stderr) = TokioChildProcess::builder(cmd)
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .with_context(|| format!("Failed to spawn MCP server process: {command}"))?;
                if let Some(stderr) = stderr {
                    forward_stderr(self.name.clone(), stderr);
                }
                handler.serve_with_lifecycle(transport, lifecycle).await?
            }
        };
        Ok(service)
    }

    /// Keep `tools_changed` fed on a 2026-07-28 server.
    ///
    /// A legacy server pushes `notifications/tools/list_changed` down the
    /// connection unasked, and [`AgentClientHandler::on_tool_list_changed`]
    /// catches it. A 2026-07-28 server sends notifications only on a
    /// `subscriptions/listen` stream someone opened, so open one — for a
    /// server that advertises the notification, and only then.
    fn watch_tool_list(&self, service: &Service) -> Option<JoinHandle<()>> {
        let info = service.peer().peer_info()?;
        if info.protocol_version.has_initialize() {
            return None;
        }
        let advertised = info
            .capabilities
            .tools
            .as_ref()
            .and_then(|t| t.list_changed)
            .unwrap_or(false);
        if !advertised {
            return None;
        }
        let peer = service.peer().clone();
        let name = self.name.clone();
        let tools_changed = Arc::clone(&self.tools_changed);
        Some(tokio::spawn(async move {
            let filter = SubscriptionFilter::builder().tools_list_changed().build();
            let mut subscription = match peer.listen(filter).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("MCP '{name}': could not subscribe to tool-list changes: {e}");
                    return;
                }
            };
            loop {
                match subscription.next().await {
                    Ok(Some(ServerNotification::ToolListChangedNotification(_))) => {
                        info!("MCP '{name}': tool list changed, will refresh");
                        tools_changed.store(true, Ordering::Relaxed);
                    }
                    Ok(Some(other)) => debug!("MCP '{name}': notification: {other:?}"),
                    Ok(None) => break,
                    Err(e) => {
                        warn!("MCP '{name}': tool-list subscription failed: {e}");
                        break;
                    }
                }
            }
            debug!(
                "MCP '{name}': tool-list subscription ended: {:?}",
                subscription.end()
            );
        }))
    }
}

/// Log a stdio server's stderr line by line instead of letting it write
/// over the terminal the agent may be drawing on.
fn forward_stderr(name: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!("MCP '{name}' stderr: {line}");
        }
    });
}

/// Whether a handshake failed only because the server answered
/// `server/discover` with versions that all still use `initialize`.
fn offers_only_legacy(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ClientInitializeError>(),
        Some(ClientInitializeError::NoCompatibleProtocolVersion { server_supported, .. })
            if !server_supported.is_empty()
                && server_supported.iter().all(ProtocolVersion::has_initialize)
    )
}

/// Render a handshake failure with everything in it, and nothing else.
///
/// Two of rmcp's messages need help. `LegacyFallbackFailed` says only
/// "discover and legacy initialize both failed"; the two failures are the
/// diagnosis. A transport error leads with the transport's full generic
/// type name, which drowns the HTTP status that follows it.
fn describe_init_error(e: &ClientInitializeError) -> String {
    match e {
        ClientInitializeError::LegacyFallbackFailed { discover, fallback } => format!(
            "server/discover: {}; initialize: {}",
            describe_init_error(discover),
            describe_init_error(fallback)
        ),
        ClientInitializeError::TransportError { error, context } => {
            format!("{}: {context}", error.error)
        }
        other => other.to_string(),
    }
}

/// Render a request failure; see [`describe_init_error`] for why a
/// transport error is unwrapped.
fn describe_service_error(e: &ServiceError) -> String {
    match e {
        ServiceError::TransportSend(error) => format!("transport error: {}", error.error),
        other => other.to_string(),
    }
}

/// Flatten a tool result into the text the LLM sees: the text content
/// blocks joined by newlines, or failing that the whole result as JSON.
pub fn tool_result_text(result: &CallToolResult) -> Result<String> {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    if !texts.is_empty() {
        return Ok(texts.join("\n"));
    }
    Ok(serde_json::to_string_pretty(result)?)
}

/// The text of a resource read: the text contents joined by newlines.
/// Binary (`blob`) contents are skipped.
#[allow(dead_code)] // For the timer integration, which reads resources.
pub fn resource_text(result: &ReadResourceResult) -> String {
    result
        .contents
        .iter()
        .filter_map(|c| match c {
            ResourceContents::TextResourceContents { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// McpTool — wraps a single remote tool as a local Tool impl
// ---------------------------------------------------------------------------

/// A Tool implementation that delegates to a remote MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    spec: ToolSpec,
    remote_tool_name: String,
    /// Resolved from the server's `trust` at build time rather than read
    /// from the client on every call: the classification must not change
    /// under a tool that is already mid-permission-check.
    kind: ToolKind,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn kind(&self) -> ToolKind {
        self.kind
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let result = self.client.call_tool(&self.remote_tool_name, input).await?;
        tool_result_text(&result)
    }
}

// ---------------------------------------------------------------------------
// Factory helpers
// ---------------------------------------------------------------------------

/// The `ToolKind` an operator's `trust` declaration buys a server's tools.
///
/// `None` maps to `Other` — not because MCP is dangerous, but because
/// `Other` is what "unclassified" means here, and the policy's strictest
/// bucket is the right place for it.
fn kind_for_trust(trust: McpTrust) -> ToolKind {
    match trust {
        McpTrust::None => ToolKind::Other,
        McpTrust::Read => ToolKind::Read,
        McpTrust::Edit => ToolKind::Edit,
    }
}

/// Build `McpTool` instances from a connected client's tool list.
pub fn build_tools_for_client(
    client: &Arc<McpClient>,
    remote_tools: Vec<RemoteToolSpec>,
) -> Vec<Box<dyn Tool>> {
    let kind = kind_for_trust(client.trust());
    remote_tools
        .into_iter()
        .map(|rt| {
            let tool_name = format!("mcp__{}__{}", client.name(), rt.name);
            Box::new(McpTool {
                client: Arc::clone(client),
                spec: ToolSpec {
                    name: Cow::Owned(tool_name),
                    description: Cow::Owned(rt.description),
                    input_schema: rt.input_schema,
                },
                remote_tool_name: rt.name,
                kind,
            }) as Box<dyn Tool>
        })
        .collect()
}

/// Connect to all configured MCP servers.  Returns `(tools, clients)`.
///
/// The clients are needed later to check `tools_changed` and refresh the
/// tool set dynamically via `ToolSet::refresh_if_needed`.
pub async fn create_mcp_tools(
    configs: &[McpServerConfig],
    workspace_root: &str,
) -> (Vec<Box<dyn Tool>>, Vec<Arc<McpClient>>) {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut clients: Vec<Arc<McpClient>> = Vec::new();

    for config in configs {
        let client = Arc::new(McpClient::new(config, workspace_root));

        if let Err(e) = client.connect().await {
            warn!("MCP '{}': failed to connect: {e:#}", config.name);
            continue;
        }

        match client.list_tools().await {
            Ok(remote_tools) => {
                tools.extend(build_tools_for_client(&client, remote_tools));
            }
            Err(e) => {
                warn!("MCP '{}': failed to list tools: {e:#}", config.name);
            }
        }

        clients.push(client);
    }

    (tools, clients)
}

#[cfg(test)]
mod tests;
