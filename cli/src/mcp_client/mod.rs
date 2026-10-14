//! Built-in MCP client for connecting to external MCP servers.
//!
//! Each configured MCP server's tools are registered in the agent's `ToolSet`
//! using the naming convention `mcp__<server_name>__<tool_name>`.

pub mod transport;

use crate::config::{McpServerConfig, McpTransportConfig, McpTrust};
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use transport::{
    HttpTransport, McpTransport, NotificationHandler, ServerRequestHandler, SessionExpired,
    StdioTransport,
};

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
// MCP Client
// ---------------------------------------------------------------------------

/// Client for a single external MCP server.
pub struct McpClient {
    name: String,
    config: McpServerConfig,
    transport: tokio::sync::RwLock<Arc<dyn McpTransport>>,
    workspace_root: String,
    request_id: Mutex<u64>,
    /// Set to `true` when the server sends `notifications/tools/list_changed`.
    tools_changed: Arc<AtomicBool>,
}

impl McpClient {
    /// Create a new client and establish the transport.
    pub async fn new(config: &McpServerConfig, workspace_root: &str) -> Result<Self> {
        let transport = Self::build_transport(&config.transport).await?;

        Ok(Self {
            name: config.name.clone(),
            config: config.clone(),
            transport: tokio::sync::RwLock::new(transport),
            workspace_root: workspace_root.to_string(),
            request_id: Mutex::new(1),
            tools_changed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Build a new transport instance from the config.
    async fn build_transport(transport: &McpTransportConfig) -> Result<Arc<dyn McpTransport>> {
        Ok(match transport {
            McpTransportConfig::Http { url, api_key } => {
                Arc::new(HttpTransport::new(url.clone(), api_key.clone()))
            }
            McpTransportConfig::Stdio { command, args, env } => {
                Arc::new(StdioTransport::new(command, args, env).await?)
            }
        })
    }

    /// Tear down the existing transport and establish a fresh one.
    ///
    /// The request-id counter resets to 1 (the new session starts fresh).
    /// On failure the old transport is already gone; the caller may retry.
    pub async fn reconnect(&self) -> Result<()> {
        info!("MCP '{}': reconnecting", self.name);

        // Shut down the old transport first so we don't leak a child process
        // if the new transport fails to spawn.
        {
            let old = self.transport.read().await.clone();
            if let Err(e) = old.shutdown().await {
                warn!(
                    "MCP '{}': shutdown during reconnect failed: {e:#}",
                    self.name
                );
            }
        }

        let new_transport = Self::build_transport(&self.config.transport)
            .await
            .with_context(|| format!("MCP '{}': failed to build new transport", self.name))?;
        *self.transport.write().await = new_transport;
        *self.request_id.lock().await = 1;
        self.tools_changed.store(false, Ordering::Relaxed);

        self.connect().await?;
        Ok(())
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

    /// Get the next request ID.
    async fn next_id(&self) -> u64 {
        let mut id = self.request_id.lock().await;
        let current = *id;
        *id += 1;
        current
    }

    /// Build the server-request handler that handles Elicitation, Roots, and
    /// Sampling callbacks from the MCP server.
    fn server_request_handler(&self) -> ServerRequestHandler {
        let workspace_root = self.workspace_root.clone();
        Arc::new(move |method: &str, params: &Value| -> Value {
            match method {
                "roots/list" => {
                    json!({
                        "result": {
                            "roots": [{
                                "uri": format!("file://{workspace_root}"),
                                "name": "workspace"
                            }]
                        }
                    })
                }
                "elicitation/create" => {
                    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    json!({
                        "result": {
                            "action": "accept",
                            "content": message
                        }
                    })
                }
                "sampling/createMessage" => {
                    json!({
                        "error": {
                            "code": -32601,
                            "message": "Sampling is not supported by this client"
                        }
                    })
                }
                _ => {
                    json!({
                        "error": {
                            "code": -32601,
                            "message": format!("Unknown method: {method}")
                        }
                    })
                }
            }
        })
    }

    /// Build the notification handler that watches for `tools/list_changed`.
    fn notification_handler(&self) -> NotificationHandler {
        let tools_changed = Arc::clone(&self.tools_changed);
        let name = self.name.clone();
        Arc::new(move |method: &str, _params: &Value| {
            debug!("MCP '{name}': notification: {method}");
            if method == "notifications/tools/list_changed" {
                info!("MCP '{name}': tool list changed, will refresh");
                tools_changed.store(true, Ordering::Relaxed);
            }
        })
    }

    /// Send a JSON-RPC request, re-establishing the session once if the
    /// server has forgotten it.
    ///
    /// A Streamable HTTP server drops idle sessions — `rmcp` after five
    /// minutes — and the id this client holds is only ever refreshed by a
    /// handshake, which until now happened exactly once at startup. So a
    /// quiet afternoon was enough to turn every later tool call into a
    /// permanent failure that nothing but a restart or a hand-run
    /// `mcp_reconnect` could clear.
    ///
    /// The retry is deliberately once. A fresh session being rejected too is
    /// not staleness, and repeating the handshake against a server that is
    /// down or misconfigured only buries the real error.
    async fn send(&self, method: &str, params: Value) -> Result<Value> {
        match self.send_once(method, params.clone()).await {
            Err(e) if e.downcast_ref::<SessionExpired>().is_some() => {
                info!(
                    "MCP '{}': {method} found the session gone; re-establishing it",
                    self.name
                );
                self.connect().await.with_context(|| {
                    format!("MCP '{}': failed to re-establish the session", self.name)
                })?;
                self.send_once(method, params).await
            }
            other => other,
        }
    }

    /// Send a JSON-RPC request through the transport, exactly once.
    ///
    /// [`Self::connect`] uses this rather than [`Self::send`]: the recovery
    /// path *is* the handshake, and routing the handshake back through it
    /// would recurse.
    async fn send_once(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id().await;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let req_handler = self.server_request_handler();
        let notif_handler = self.notification_handler();
        let transport = self.transport.read().await.clone();
        let response = transport
            .request(&body, &req_handler, &notif_handler)
            .await?;

        if let Some(err) = response.get("error") {
            let msg = err["message"].as_str().unwrap_or("unknown error");
            let code = err["code"].as_i64().unwrap_or(-1);
            bail!("MCP server error {code}: {msg}");
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Initialize the MCP session (handshake).
    pub async fn connect(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "roots": { "listChanged": false },
                "elicitation": {}
            },
            "clientInfo": {
                "name": "sapphire-agent",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_once("initialize", params).await?;
        info!(
            "MCP '{}': connected (server: {})",
            self.name,
            result
                .get("serverInfo")
                .and_then(|s| s.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
        );

        // Send initialized notification (no id, no response expected).
        // Must use `notify` — `request` would block reading stdout for a
        // response that the spec says will never arrive.
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        let transport = self.transport.read().await.clone();
        if let Err(e) = transport.notify(&notification).await {
            warn!(
                "MCP '{}': initialized notification failed: {e:#}",
                self.name
            );
        }

        Ok(())
    }

    /// List tools available on the remote MCP server.
    pub async fn list_tools(&self) -> Result<Vec<RemoteToolSpec>> {
        let result = self.send("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let specs: Vec<RemoteToolSpec> = tools
            .into_iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let description = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_schema = t.get("inputSchema").cloned().unwrap_or(json!({}));
                Some(RemoteToolSpec {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect();

        info!("MCP '{}': found {} tools", self.name, specs.len());
        Ok(specs)
    }

    /// Call a tool on the remote MCP server.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Value> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        self.send("tools/call", params).await
    }

    /// Shut down the transport.
    #[allow(dead_code)]
    pub async fn shutdown(&self) -> Result<()> {
        let transport = self.transport.read().await.clone();
        transport.shutdown().await
    }
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

        // MCP tools/call returns { content: [...] } where each item has
        // type "text" with a text field.  Concatenate all text content.
        if let Some(contents) = result.get("content").and_then(|c| c.as_array()) {
            let texts: Vec<&str> = contents
                .iter()
                .filter_map(|c| {
                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                        c.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            if !texts.is_empty() {
                return Ok(texts.join("\n"));
            }
        }

        // Fallback: pretty-print the raw result.
        Ok(serde_json::to_string_pretty(&result)?)
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
        let client = match McpClient::new(config, workspace_root).await {
            Ok(c) => Arc::new(c),
            Err(e) => {
                warn!("MCP '{}': failed to create client: {e:#}", config.name);
                continue;
            }
        };

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
mod tests {
    use super::*;

    /// A client over the HTTP transport, which connects lazily — nothing
    /// here reaches the network.
    async fn client_with(trust: McpTrust) -> Arc<McpClient> {
        let config = McpServerConfig {
            name: "ledger".to_string(),
            transport: McpTransportConfig::Http {
                url: "http://127.0.0.1:1/mcp".to_string(),
                api_key: None,
            },
            trust,
        };
        Arc::new(
            McpClient::new(&config, "/tmp")
                .await
                .expect("http transport is lazy"),
        )
    }

    fn remote(name: &str) -> RemoteToolSpec {
        RemoteToolSpec {
            name: name.to_string(),
            description: "a remote tool".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn kinds_for(trust: McpTrust) -> Vec<(String, ToolKind)> {
        let client = client_with(trust).await;
        build_tools_for_client(&client, vec![remote("list"), remote("record")])
            .iter()
            .map(|t| (t.spec().name.to_string(), t.kind()))
            .collect()
    }

    /// The default, and the behaviour every existing config keeps: the
    /// server's tools are `Other`, the strictest bucket, so a channel
    /// refuses them.
    #[tokio::test]
    async fn an_untrusted_server_classifies_its_tools_as_other() {
        for (name, kind) in kinds_for(McpTrust::None).await {
            assert_eq!(kind, ToolKind::Other, "{name}");
        }
    }

    #[tokio::test]
    async fn a_read_trusted_server_classifies_its_tools_as_read() {
        for (name, kind) in kinds_for(McpTrust::Read).await {
            assert_eq!(kind, ToolKind::Read, "{name}");
        }
    }

    #[tokio::test]
    async fn an_edit_trusted_server_classifies_its_tools_as_edit() {
        for (name, kind) in kinds_for(McpTrust::Edit).await {
            assert_eq!(kind, ToolKind::Edit, "{name}");
        }
    }

    /// The point of the field, stated as the policy outcome rather than
    /// the classification: an untrusted server stays unreachable from a
    /// channel — which is the heartbeat's chat leg too — and a trusted
    /// one becomes reachable, without `decide` changing at all.
    #[tokio::test]
    async fn trust_decides_whether_a_channel_can_reach_the_server() {
        use crate::tools::policy::{Decision, Origin, SessionMode, decide};

        let kind = |kinds: Vec<(String, ToolKind)>| kinds[0].1;

        let untrusted = kind(kinds_for(McpTrust::None).await);
        assert_eq!(decide(Origin::Channel, untrusted), Decision::Deny);

        let read = kind(kinds_for(McpTrust::Read).await);
        assert_eq!(decide(Origin::Channel, read), Decision::Allow);
        assert_eq!(
            decide(Origin::Acp(SessionMode::Default), read),
            Decision::Allow,
            "a read is safe on every origin"
        );

        let edit = kind(kinds_for(McpTrust::Edit).await);
        assert_eq!(
            decide(Origin::Channel, edit),
            Decision::Allow,
            "chat-driven recording is the case this unblocks"
        );
        assert_eq!(
            decide(Origin::Acp(SessionMode::Default), edit),
            Decision::Ask,
            "an editor still asks before a write, as it does for file_write"
        );
    }

    /// The classification is per server, not per tool: `trust` is coarse
    /// on purpose, so a tool the server adds later cannot silently fall
    /// back to `Other` and look like the server being broken.
    #[tokio::test]
    async fn trust_applies_to_every_tool_the_server_lists() {
        let kinds = kinds_for(McpTrust::Edit).await;
        let names: Vec<&str> = kinds.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["mcp__ledger__list", "mcp__ledger__record"]);
    }
}
