use super::*;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use rmcp::model::{
    CallToolResponse, ContentBlock, ListToolsResult, PaginatedRequestParams, ReadResourceResponse,
    ServerCapabilities, ServerConfig, Tool as RemoteTool, ToolListChangedNotification,
};
use rmcp::service::{RoleServer, SubscriptionContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::json;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Trust → classification
// ---------------------------------------------------------------------------

/// A client over the HTTP transport. Construction connects nothing, so
/// nothing here reaches the network.
fn client_with(trust: McpTrust) -> Arc<McpClient> {
    let config = McpServerConfig {
        name: "ledger".to_string(),
        transport: McpTransportConfig::Http {
            url: "http://127.0.0.1:1/mcp".to_string(),
            api_key: None,
        },
        trust,
    };
    Arc::new(McpClient::new(&config, "/tmp"))
}

fn remote(name: &str) -> RemoteToolSpec {
    RemoteToolSpec {
        name: name.to_string(),
        description: "a remote tool".to_string(),
        input_schema: json!({"type": "object"}),
    }
}

fn kinds_for(trust: McpTrust) -> Vec<(String, ToolKind)> {
    let client = client_with(trust);
    build_tools_for_client(&client, vec![remote("list"), remote("record")])
        .iter()
        .map(|t| (t.spec().name.to_string(), t.kind()))
        .collect()
}

/// The default, and the behaviour every existing config keeps: the
/// server's tools are `Other`, the strictest bucket, so a channel
/// refuses them.
#[test]
fn an_untrusted_server_classifies_its_tools_as_other() {
    for (name, kind) in kinds_for(McpTrust::None) {
        assert_eq!(kind, ToolKind::Other, "{name}");
    }
}

#[test]
fn a_read_trusted_server_classifies_its_tools_as_read() {
    for (name, kind) in kinds_for(McpTrust::Read) {
        assert_eq!(kind, ToolKind::Read, "{name}");
    }
}

#[test]
fn an_edit_trusted_server_classifies_its_tools_as_edit() {
    for (name, kind) in kinds_for(McpTrust::Edit) {
        assert_eq!(kind, ToolKind::Edit, "{name}");
    }
}

/// The point of the field, stated as the policy outcome rather than
/// the classification: an untrusted server stays unreachable from a
/// channel — which is the heartbeat's chat leg too — and a trusted
/// one becomes reachable, without `decide` changing at all.
#[test]
fn trust_decides_whether_a_channel_can_reach_the_server() {
    use crate::tools::policy::{Decision, Origin, SessionMode, decide};

    let kind = |kinds: Vec<(String, ToolKind)>| kinds[0].1;

    let untrusted = kind(kinds_for(McpTrust::None));
    assert_eq!(decide(Origin::Channel, untrusted), Decision::Deny);

    let read = kind(kinds_for(McpTrust::Read));
    assert_eq!(decide(Origin::Channel, read), Decision::Allow);
    assert_eq!(
        decide(Origin::Acp(SessionMode::Default), read),
        Decision::Allow,
        "a read is safe on every origin"
    );

    let edit = kind(kinds_for(McpTrust::Edit));
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
#[test]
fn trust_applies_to_every_tool_the_server_lists() {
    let kinds = kinds_for(McpTrust::Edit);
    let names: Vec<&str> = kinds.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["mcp__ledger__list", "mcp__ledger__record"]);
}

// ---------------------------------------------------------------------------
// Against a real server
// ---------------------------------------------------------------------------

/// An in-process MCP server: one tool, one resource, and a
/// `subscriptions/listen` that reports a tool-list change straight away.
#[derive(Clone)]
struct Ledger {
    /// The protocol versions it admits to; capping this below 2026-07-28
    /// makes it a legacy server that still answers `server/discover`.
    versions: &'static [ProtocolVersion],
}

impl ServerHandler for Ledger {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .build(),
        )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(self.versions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let schema = json!({"type": "object"});
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![RemoteTool::new(
            "list_accounts",
            "List the ledger's accounts",
            schema,
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let filter = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("filter"))
            .and_then(Value::as_str)
            .unwrap_or("*")
            .to_string();
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("accounts matching {filter}")),
        ])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text("next: 09:00", request.uri)],
        )))
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let _ = context
            .sink()
            .send(ServerNotification::ToolListChangedNotification(
                ToolListChangedNotification::default(),
            ))
            .await;
        context.cancelled().await;
        Ok(())
    }
}

/// What the server-side middleware saw, in arrival order.
#[derive(Clone, Default)]
struct Seen {
    /// The `authorization` header of every request.
    auth: Arc<StdMutex<Vec<Option<String>>>>,
    /// The session ids the server has handed out.
    sessions: Arc<StdMutex<Vec<String>>>,
    /// While set, any request carrying the first session id gets `404` —
    /// the answer a Streamable HTTP server gives for a session it dropped.
    forget_first: Arc<AtomicBool>,
}

async fn observe(State(seen): State<Seen>, request: Request, next: Next) -> Response {
    let header = |headers: &HeaderMap, name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    seen.auth
        .lock()
        .expect("auth")
        .push(header(request.headers(), "authorization"));

    let session = header(request.headers(), "mcp-session-id");
    let first = seen.sessions.lock().expect("sessions").first().cloned();
    if seen.forget_first.load(Ordering::Relaxed) && session.is_some() && session == first {
        return (StatusCode::NOT_FOUND, "Not Found: Session not found").into_response();
    }

    let response = next.run(request).await;
    if let Some(minted) = header(response.headers(), "mcp-session-id") {
        let mut sessions = seen.sessions.lock().expect("sessions");
        if !sessions.contains(&minted) {
            sessions.push(minted);
        }
    }
    response
}

/// Bind a loopback port, serve `router` on it, and return the `/mcp` URL.
///
/// The task is left running: every test here finishes quickly and the
/// port dies with the test binary.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}/mcp")
}

/// Serve [`Ledger`] the way a sapphire-framework app does: rmcp's
/// Streamable HTTP service at `/mcp`, with session support for legacy
/// clients.
async fn serve_ledger(ledger: Ledger, seen: &Seen) -> String {
    let service = StreamableHttpService::new(
        move || Ok(ledger.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(seen.clone(), observe));
    serve(router).await
}

fn http_config(url: String, api_key: Option<&str>) -> McpServerConfig {
    McpServerConfig {
        name: "ledger".to_string(),
        trust: McpTrust::Read,
        transport: McpTransportConfig::Http {
            url,
            api_key: api_key.map(str::to_string),
        },
    }
}

/// The ordinary path, end to end against a current server: list, call,
/// read, every request carrying the bearer.
#[tokio::test]
async fn a_current_server_is_listed_called_and_read_with_the_bearer() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::KNOWN_VERSIONS,
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, Some("secret")), "/tmp");

    client.connect().await.expect("connect");
    let tools = client.list_tools().await.expect("list tools");
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["list_accounts"]
    );
    assert_eq!(tools[0].input_schema, json!({"type": "object"}));

    let result = client
        .call_tool("list_accounts", &json!({"filter": "assets"}))
        .await
        .expect("call tool");
    assert_eq!(
        tool_result_text(&result).expect("text"),
        "accounts matching assets"
    );

    let resource = client
        .read_resource("ledger://schedule")
        .await
        .expect("read resource");
    assert_eq!(resource_text(&resource), "next: 09:00");

    let auth = seen.auth.lock().expect("auth");
    assert!(!auth.is_empty());
    assert!(
        auth.iter().all(|a| a.as_deref() == Some("Bearer secret")),
        "every request must authenticate: {auth:?}"
    );
    assert!(
        seen.sessions.lock().expect("sessions").is_empty(),
        "a 2026-07-28 exchange is stateless"
    );
}

/// `listen` hands back the subscription, and the notification the server
/// sends on it.
#[tokio::test]
async fn a_subscription_delivers_the_servers_notifications() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::KNOWN_VERSIONS,
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, None), "/tmp");
    client.connect().await.expect("connect");

    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .expect("listen");
    assert_eq!(subscription.acknowledged().tools_list_changed, Some(true));

    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("a notification in time")
        .expect("no subscription error");
    assert!(
        matches!(
            notification,
            Some(ServerNotification::ToolListChangedNotification(_))
        ),
        "got {notification:?}"
    );
}

/// A 2026-07-28 server never pushes `tools/list_changed` unasked, so the
/// client subscribes for it; the flag the tool set polls must still rise.
#[tokio::test]
async fn a_tool_list_change_on_a_current_server_raises_the_flag() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::KNOWN_VERSIONS,
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, None), "/tmp");
    client.connect().await.expect("connect");

    let raised = tokio::time::timeout(Duration::from_secs(5), async {
        while !client.take_tools_changed() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(raised.is_ok(), "the watcher must set tools_changed");
}

/// A server that answers `server/discover` but offers only versions with
/// `initialize` must still be reached — through `initialize`.
#[tokio::test]
async fn a_legacy_server_is_reached_through_initialize() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::known_up_to(&ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, Some("secret")), "/tmp");

    client.connect().await.expect("connect falls back");
    let result = client
        .call_tool("list_accounts", &json!({}))
        .await
        .expect("call tool");
    assert_eq!(
        tool_result_text(&result).expect("text"),
        "accounts matching *"
    );
    assert_eq!(
        seen.sessions.lock().expect("sessions").len(),
        1,
        "a legacy exchange runs in a session"
    );
}

/// The failure that motivated session recovery: the handshake succeeds, the
/// server forgets the session, and the next call must still return a result.
/// rmcp's transport re-runs `initialize` and replays the call itself.
#[tokio::test]
async fn an_expired_legacy_session_is_re_established_and_the_call_retried() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::known_up_to(&ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, None), "/tmp");
    client.connect().await.expect("connect");

    seen.forget_first.store(true, Ordering::Relaxed);
    let result = client
        .call_tool("list_accounts", &json!({}))
        .await
        .expect("a forgotten session must be re-established, not reported");
    assert_eq!(
        tool_result_text(&result).expect("text"),
        "accounts matching *"
    );
    assert_eq!(
        seen.sessions.lock().expect("sessions").len(),
        2,
        "the recovery mints a new session"
    );
}

/// A connection that has been closed is replaced by the next call, not
/// reported as closed until someone runs `mcp_reconnect`.
#[tokio::test]
async fn a_closed_connection_is_re_established_by_the_next_call() {
    let seen = Seen::default();
    let ledger = Ledger {
        versions: ProtocolVersion::KNOWN_VERSIONS,
    };
    let url = serve_ledger(ledger, &seen).await;
    let client = McpClient::new(&http_config(url, None), "/tmp");
    client.connect().await.expect("connect");

    client.shutdown().await.expect("shutdown");
    client
        .call_tool("list_accounts", &json!({}))
        .await
        .expect("the next call reconnects");
}

/// Answer every POST with one canned response.
async fn serving(status: StatusCode, content_type: &'static str, body: &'static str) -> String {
    let router = Router::new().route(
        "/mcp",
        post(move || async move {
            let mut response = (status, body).into_response();
            response
                .headers_mut()
                .insert("content-type", HeaderValue::from_static(content_type));
            response
        }),
    );
    serve(router).await
}

/// `401` is what an unauthenticated request gets, and its body is empty.
/// The status is the entire diagnosis, so it has to reach the caller.
#[tokio::test]
async fn an_error_status_names_itself() {
    let url = serving(StatusCode::UNAUTHORIZED, "text/plain", "").await;
    let client = McpClient::new(&http_config(url, None), "/tmp");

    let err = client.connect().await.expect_err("401 is not a handshake");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("401"),
        "the status must be in the message: {msg}"
    );
}

/// rmcp answers a malformed request with `400` and a JSON-RPC error
/// object. That object is the server answering, and its `message` is the
/// useful part — it must reach the caller.
#[tokio::test]
async fn a_json_rpc_error_carried_on_a_4xx_still_reaches_the_caller() {
    let url = serving(
        StatusCode::BAD_REQUEST,
        "application/json",
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"unsupported protocol version"}}"#,
    )
    .await;
    let client = McpClient::new(&http_config(url, None), "/tmp");

    let err = client
        .connect()
        .await
        .expect_err("a 400 fails the handshake");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unsupported protocol version"),
        "the server's own message is the useful part: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Handler answers
// ---------------------------------------------------------------------------

/// A server asking a 2026-07-28 client for input does it through rounds of
/// the call; these are the answers it gets, whatever the transport.
#[test]
fn the_client_declares_roots_and_elicitation_but_not_sampling() {
    let handler = AgentClientHandler {
        name: "ledger".to_string(),
        workspace_root: "/srv/ws".to_string(),
        tools_changed: Arc::new(AtomicBool::new(false)),
    };
    let info = ClientHandler::get_info(&handler);
    #[allow(deprecated)]
    let (roots, sampling) = (
        info.capabilities.roots.is_some(),
        info.capabilities.sampling.is_some(),
    );
    assert!(roots);
    assert!(info.capabilities.elicitation.is_some());
    assert!(!sampling);
}
