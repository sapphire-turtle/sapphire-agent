//! MCP servers an ACP client provides itself (`"type": "acp"`).
//!
//! A client may list, in `session/new`'s (or `load`'s, or `resume`'s)
//! `mcpServers`, servers that live inside the client and are reached only
//! through `mcp/message` on the ACP connection (the MCP-over-ACP RFD). Their
//! tools become the session's own, `acp__<server>__<tool>`, beside the
//! agent's configured ones.
//!
//! Two pieces, one per lifetime:
//! - [`AcpMcpPeer`], one per connection: sends each `mcp/message` request,
//!   routes the client's `mcp/message` notifications to the request they
//!   belong to, and fails everything in flight the moment the connection
//!   is torn down.
//! - [`ClientMcp`], one per session: an [`McpClient`] per offered ACP
//!   server, and the [`ToolSet`] their tools are listed into, in the
//!   background. A prompt waits for that listing, but only so long.

use crate::mcp_client::acp_transport::{McpCarrier, McpOverAcpError, McpOverAcpPeer};
use crate::mcp_client::{McpClient, sanitize_tool_segment};
use crate::tools::ToolSet;
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    McpServer, MessageMcpNotification, MessageMcpRequest, MessageMcpResponse,
};
use agent_client_protocol::{Client, is_connection_terminated, is_incoming_transport_closed};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How long a prompt waits for the session's client tools to be listed.
/// Past it the turn runs with whatever has arrived; the rest join later
/// turns once their listing finishes.
pub(crate) const CLIENT_MCP_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// JSON-RPC's "method not found": what a client that does not implement
/// `mcp/message` answers it with.
const METHOD_NOT_FOUND: i64 = -32601;

/// An `mcp/message` request in flight: `(server_id, request_id)`.
type RouteKey = (String, String);

/// The ACP connection as an [`McpOverAcpPeer`]. One per connection, shared
/// by its sessions.
pub(crate) struct AcpMcpPeer {
    connection: ConnectionTo<Client>,
    /// Where the client's notifications for each request in flight go.
    /// An entry lives as long as its request: removed when the request
    /// ends, and in any case once its receiver is gone.
    routes: Mutex<HashMap<RouteKey, mpsc::UnboundedSender<(String, Value)>>>,
    /// Fired by [`Self::close`]. Every request in flight selects on it.
    closed: CancellationToken,
}

impl AcpMcpPeer {
    pub(crate) fn new(connection: ConnectionTo<Client>) -> Arc<Self> {
        Arc::new(Self {
            connection,
            routes: Mutex::default(),
            closed: CancellationToken::new(),
        })
    }

    /// Route one `mcp/message` notification from the client. Unknown
    /// `(server_id, request_id)`: dropped, debug log — the RFD scopes
    /// notifications to a request still active, and one that raced its
    /// request's end has nobody left to read it.
    pub(crate) fn deliver(&self, notif: MessageMcpNotification) {
        let key = (
            notif.server_id.0.to_string(),
            notif.request_id.0.to_string(),
        );
        let params = notif.params.map_or_else(|| json!({}), Value::Object);
        let mut routes = self.routes.lock().unwrap();
        let delivered = routes
            .get(&key)
            .map(|tx| tx.send((notif.method.clone(), params)).is_ok());
        match delivered {
            Some(true) => {}
            Some(false) => {
                routes.remove(&key);
                debug!(
                    "ACP: dropped mcp/message {} for {}/{}: its request has ended",
                    notif.method, key.0, key.1
                );
            }
            None => debug!(
                "ACP: dropped mcp/message {} for {}/{}: no such request in flight",
                notif.method, key.0, key.1
            ),
        }
    }

    /// Called from connection teardown: every request in flight resolves
    /// to `Err(Closed)` at once, and no new one goes out.
    pub(crate) fn close(&self) {
        self.closed.cancel();
        self.routes.lock().unwrap().clear();
    }

    fn unroute(&self, key: &RouteKey) {
        self.routes.lock().unwrap().remove(key);
    }
}

/// Removes a request's notification route however the request ends —
/// answered, failed, or its future dropped by a cancel.
struct RouteGuard<'a> {
    peer: &'a AcpMcpPeer,
    key: RouteKey,
}

impl Drop for RouteGuard<'_> {
    fn drop(&mut self) {
        self.peer.unroute(&self.key);
    }
}

#[async_trait::async_trait]
impl McpOverAcpPeer for AcpMcpPeer {
    async fn request(
        &self,
        server_id: &str,
        request_id: &str,
        method: &str,
        params: Value,
    ) -> Result<McpCarrier, McpOverAcpError> {
        let _route = RouteGuard {
            peer: self,
            key: (server_id.to_string(), request_id.to_string()),
        };
        if self.is_closed() {
            return Err(McpOverAcpError::Closed);
        }
        let params = match params {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                debug!("ACP: mcp/message {method}: non-object params dropped: {other}");
                None
            }
        };
        let request = MessageMcpRequest::new(server_id.to_string(), request_id.to_string(), method)
            .params(params);
        // Awaited outside the dispatch loop: every caller is an rmcp
        // transport task (`AcpMcpTransport`), never a request handler.
        // Dropping this future (a cancelled call) drops the `SentRequest`
        // inside it, which sends `$/cancel_request`.
        let response = self.connection.send_request(request).block_task();
        tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(McpOverAcpError::Closed),
            response = response => match response {
                Ok(MessageMcpResponse::Result { result, .. }) => Ok(McpCarrier::Result(result)),
                Ok(MessageMcpResponse::Error { error, .. }) => Ok(McpCarrier::Error {
                    code: error.code.into(),
                    message: error.message,
                    data: match error.data {
                        MaybeUndefined::Undefined => None,
                        MaybeUndefined::Null => Some(Value::Null),
                        MaybeUndefined::Value(data) => Some(data),
                    },
                }),
                // `#[non_exhaustive]`: an outcome this build cannot read.
                Ok(_) => Err(McpOverAcpError::Outer {
                    code: -32603,
                    message: "unrecognised mcp/message response".to_string(),
                }),
                Err(e) if is_connection_terminated(&e) || is_incoming_transport_closed(&e) => {
                    Err(McpOverAcpError::Closed)
                }
                Err(e) => Err(McpOverAcpError::Outer {
                    code: i32::from(e.code).into(),
                    message: e.message,
                }),
            },
        }
    }

    fn notifications(
        &self,
        server_id: &str,
        request_id: &str,
    ) -> mpsc::UnboundedReceiver<(String, Value)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut routes = self.routes.lock().unwrap();
        // A subscription whose request never went out leaves its sender
        // behind with nobody to remove it; sweep those here.
        routes.retain(|_, tx| !tx.is_closed());
        if !self.closed.is_cancelled() {
            routes.insert((server_id.to_string(), request_id.to_string()), tx);
        }
        rx
    }

    fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }
}

/// Per-session client tools, filled in the background.
pub(crate) struct ClientMcp {
    tools: Arc<ToolSet>,
    ready: watch::Receiver<bool>,
    /// Set once a `tools()` call has waited (to the end of the listing or
    /// to the timeout). Only the first turn waits; later ones take the set
    /// as it stands, and tools that arrive late join the turns after.
    waited: AtomicBool,
    /// The listing task, stopped when the session lets go of this — a
    /// `session/load` replacing it, or the connection ending.
    listing: Option<AbortHandle>,
}

impl ClientMcp {
    /// Pick the `acp` servers out of `servers`, warn about the rest, name
    /// them (sanitize + `_2`, `_3` on collision), and spawn the
    /// `tools/list` of each.
    ///
    /// The peer is the connection's [`AcpMcpPeer`]; any [`McpOverAcpPeer`]
    /// does, which is what lets the tests script one.
    pub(crate) fn start(
        servers: &[McpServer],
        peer: Arc<dyn McpOverAcpPeer>,
        workspace_root: &str,
        session_id: &str,
    ) -> Self {
        let offered: Vec<_> = servers
            .iter()
            .filter_map(|server| match server {
                McpServer::Acp(acp) => Some(acp),
                _ => None,
            })
            .collect();
        let ignored = servers.len() - offered.len();
        if ignored > 0 {
            // Stdio, http and sse servers are the client's to launch or
            // dial on its own machine; this agent may run elsewhere, and
            // must not spawn a command it was handed over the wire.
            warn!(
                "ACP: ignoring {ignored} non-ACP MCP server(s) offered by the client for session \
                 {session_id}: only MCP over ACP (\"type\": \"acp\") is accepted. Configure the \
                 agent's own MCP servers in [[tools.mcp_servers]]."
            );
        }
        let names = client_server_names(
            &offered
                .iter()
                .map(|acp| acp.name.as_str())
                .collect::<Vec<_>>(),
        );
        let clients: Vec<(String, Arc<McpClient>)> = offered
            .iter()
            .zip(&names)
            .map(|(acp, name)| {
                let client =
                    McpClient::over_acp(name, &acp.server_id.0, Arc::clone(&peer), workspace_root);
                (acp.name.clone(), Arc::new(client))
            })
            .collect();
        let tools = Arc::new(ToolSet::new(
            Vec::new(),
            clients.iter().map(|(_, c)| Arc::clone(c)).collect(),
        ));
        let (ready_tx, ready) = watch::channel(clients.is_empty());
        if clients.is_empty() {
            return Self {
                tools,
                ready,
                waited: AtomicBool::new(true),
                listing: None,
            };
        }

        let task = tokio::spawn({
            let tools = Arc::clone(&tools);
            let session_id = session_id.to_string();
            async move {
                // Each server's tools join the set as soon as they are
                // listed, so a prompt that stops waiting gets those.
                let listings = clients.iter().map(|(offered_name, client)| {
                    let tools = Arc::clone(&tools);
                    let session_id = session_id.as_str();
                    async move {
                        match client.list_tools().await {
                            Ok(specs) => {
                                tools.install_client_tools(client, specs).await;
                            }
                            Err(e) => warn!("{}", listing_failure(offered_name, session_id, &e)),
                        }
                    }
                });
                futures_util::future::join_all(listings).await;
                let _ = ready_tx.send(true);
            }
        });
        Self {
            tools,
            ready,
            waited: AtomicBool::new(false),
            listing: Some(task.abort_handle()),
        }
    }

    /// The tool set once ready, or after [`CLIENT_MCP_READY_TIMEOUT`] with
    /// whatever arrived. Only the first call waits: every later one
    /// returns the set as it stands at once, so a client that never
    /// answers stalls one turn, not each of them.
    pub(crate) async fn tools(&self) -> Arc<ToolSet> {
        if !self.waited.load(Ordering::Acquire) {
            let mut ready = self.ready.clone();
            // An `Err` from either is "stop waiting" too: the listing task
            // is gone (aborted, or panicked) and nothing more will arrive.
            let _ = tokio::time::timeout(CLIENT_MCP_READY_TIMEOUT, ready.wait_for(|ready| *ready))
                .await;
            self.waited.store(true, Ordering::Release);
        }
        Arc::clone(&self.tools)
    }
}

impl Drop for ClientMcp {
    fn drop(&mut self) {
        if let Some(listing) = &self.listing {
            listing.abort();
        }
    }
}

/// The warning for a client server whose tools could not be listed.
fn listing_failure(name: &str, session_id: &str, error: &anyhow::Error) -> String {
    // The outer error survives into the chain only as text: rmcp carries
    // it as an internal error whose message is `McpOverAcpError`'s
    // Display, "ACP error <code>: <message>".
    let method_not_found = McpOverAcpError::Outer {
        code: METHOD_NOT_FOUND,
        message: String::new(),
    }
    .to_string();
    if format!("{error:#}").contains(&method_not_found) {
        format!(
            "ACP: client MCP server '{name}' for session {session_id} is unavailable: the client \
             does not implement mcp/message"
        )
    } else {
        format!(
            "ACP: client MCP server '{name}' for session {session_id} is unavailable: {error:#}"
        )
    }
}

/// The agent's names for the client's servers: each sanitized into a tool
/// name segment, a repeat suffixed `_2`, `_3`, …, order kept.
pub(crate) fn client_server_names(names: &[&str]) -> Vec<String> {
    let mut taken = HashSet::new();
    names
        .iter()
        .map(|name| {
            let base = sanitize_tool_segment(name);
            let mut candidate = base.clone();
            let mut n = 2;
            while !taken.insert(candidate.clone()) {
                candidate = format!("{base}_{n}");
                n += 1;
            }
            candidate
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpClient;
    use crate::mcp_client::acp_transport::test_support::{FakePeer, FakeReply};
    use agent_client_protocol::v1::schema::{McpServerAcp, McpServerHttp, McpServerStdio};
    use std::time::Duration;

    fn acp(name: &str, id: &str) -> McpServer {
        McpServer::Acp(McpServerAcp::new(name, id.to_string()))
    }

    async fn tool_names(tools: &ToolSet) -> Vec<String> {
        tools
            .specs_filtered(|_| true)
            .await
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    /// A notification reaches the request it names while that request is
    /// in flight; a route goes when its receiver does, or its request
    /// ends; and closing fails the request in flight at once.
    #[tokio::test]
    async fn notification_routes_live_as_long_as_their_request() {
        use agent_client_protocol::{Agent, Lines};
        use futures_util::SinkExt;
        let outgoing =
            futures_util::sink::drain().sink_map_err(|e: std::convert::Infallible| match e {});
        let incoming = futures_util::stream::pending::<std::io::Result<String>>();
        Agent
            .v1()
            .connect_with(
                Lines::new(outgoing, incoming),
                async |connection: ConnectionTo<Client>| {
                    let peer = AcpMcpPeer::new(connection);
                    let progress = |request_id: &str| {
                        MessageMcpNotification::new(
                            "srv-1".to_string(),
                            request_id.to_string(),
                            "notifications/progress",
                        )
                    };
                    let routes = |peer: &AcpMcpPeer| peer.routes.lock().unwrap().len();

                    let mut rx = peer.notifications("srv-1", "r1");
                    peer.deliver(progress("r1"));
                    assert_eq!(rx.try_recv().unwrap().0, "notifications/progress");
                    // Unknown: dropped, and nothing is added for it.
                    peer.deliver(progress("nobody"));
                    assert_eq!(routes(&peer), 1);
                    drop(rx);
                    peer.deliver(progress("r1"));
                    assert_eq!(routes(&peer), 0);

                    // The client never answers; only closing ends it.
                    let _rx = peer.notifications("srv-1", "r2");
                    let request = peer.request("srv-1", "r2", "tools/list", json!({}));
                    let close = async {
                        tokio::task::yield_now().await;
                        peer.close();
                    };
                    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                        tokio::join!(request, close)
                    })
                    .await
                    .expect("closing fails the request in flight");
                    assert_eq!(outcome, Err(McpOverAcpError::Closed));
                    assert_eq!(routes(&peer), 0);
                    assert!(peer.is_closed());
                    Ok(())
                },
            )
            .await
            .unwrap();
    }

    #[test]
    fn server_names_are_sanitized_and_deduped() {
        assert_eq!(
            client_server_names(&["game", "game", "my game", "game"]),
            vec!["game", "game_2", "my_game", "game_3"]
        );
    }

    #[tokio::test]
    async fn non_acp_servers_are_ignored() {
        let peer = FakePeer::new();
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("local", "/bin/true")),
            McpServer::Http(McpServerHttp::new("remote", "http://127.0.0.1:9/mcp")),
            acp("game", "game:1"),
        ];
        let client = ClientMcp::start(&servers, peer.clone(), "/work", "s-1");
        let tools = client.tools().await;

        // Only the ACP server became a client; the stdio and http ones
        // were never dialled or spawned.
        assert_eq!(tools.mcp_server_names(), vec!["game".to_string()]);
        let lists: Vec<_> = peer
            .seen()
            .into_iter()
            .filter(|(_, m)| m == "tools/list")
            .collect();
        assert_eq!(
            lists,
            vec![("game:1".to_string(), "tools/list".to_string())]
        );
        assert!(
            peer.seen().iter().all(|(sid, _)| sid == "game:1"),
            "{:?}",
            peer.seen()
        );
        assert_eq!(tool_names(&tools).await, vec!["acp__game__poke"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_does_not_wait_past_the_ready_timeout() {
        let peer = FakePeer::new();
        peer.script("tools/list", FakeReply::Hang);
        let client = ClientMcp::start(&[acp("game", "game:1")], peer.clone(), "/work", "s-1");

        let started = tokio::time::Instant::now();
        let tools = client.tools().await;
        let waited = started.elapsed();
        assert!(waited >= CLIENT_MCP_READY_TIMEOUT, "{waited:?}");
        assert!(
            waited < CLIENT_MCP_READY_TIMEOUT + Duration::from_secs(1),
            "{waited:?}"
        );
        assert!(tool_names(&tools).await.is_empty());
    }

    /// Only the first turn waits: once one `tools()` has timed out, the
    /// next returns at once, and a late listing still lands in the set.
    #[tokio::test(start_paused = true)]
    async fn only_the_first_prompt_waits_for_the_listing() {
        let peer = FakePeer::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let listed = McpCarrier::Result(json!({
            "tools": [{
                "name": "poke",
                "inputSchema": {"type": "object"},
                "annotations": {"readOnlyHint": true}
            }]
        }));
        peer.script("tools/list", FakeReply::Held(Arc::clone(&gate), listed));
        let client = ClientMcp::start(&[acp("game", "game:1")], peer.clone(), "/work", "s-1");

        let first = client.tools().await;
        assert!(tool_names(&first).await.is_empty());

        let started = tokio::time::Instant::now();
        let second = client.tools().await;
        assert_eq!(started.elapsed(), Duration::ZERO, "the second call waited");
        assert!(tool_names(&second).await.is_empty());

        // The listing finishes later; the same set picks its tools up.
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while tool_names(&second).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the late listing lands in the set");
        assert_eq!(
            tool_names(&*client.tools().await).await,
            vec!["acp__game__poke"]
        );
    }

    /// A refresh and the initial listing racing each other install the
    /// same client's tools twice; one copy of each must remain.
    #[tokio::test]
    async fn installing_a_clients_tools_twice_leaves_one_copy() {
        let peer = FakePeer::new();
        let client = Arc::new(McpClient::over_acp("game", "game:1", peer.clone(), "/work"));
        let set = ToolSet::new(Vec::new(), vec![Arc::clone(&client)]);
        for _ in 0..2 {
            let specs = client.list_tools().await.unwrap();
            set.install_client_tools(&client, specs).await;
        }
        assert_eq!(tool_names(&set).await, vec!["acp__game__poke"]);
    }

    #[tokio::test]
    async fn a_client_without_mcp_message_still_gets_a_session() {
        let peer = FakePeer::new();
        let missing = FakeReply::Outer {
            code: -32601,
            message: "Method not found".into(),
        };
        peer.script("server/discover", missing.clone());
        peer.script("tools/list", missing);

        let client = ClientMcp::start(&[acp("game", "game:1")], peer.clone(), "/work", "s-1");
        let tools = tokio::time::timeout(Duration::from_secs(5), client.tools())
            .await
            .expect("a failed listing is ready at once, not after the timeout");
        assert!(tool_names(&tools).await.is_empty());

        // The failure is recognised as "no mcp/message here", and the
        // warning names the server and the session.
        let err = McpClient::over_acp("game", "game:1", peer.clone(), "/work")
            .list_tools()
            .await
            .unwrap_err();
        assert_eq!(
            listing_failure("game", "s-1", &err),
            "ACP: client MCP server 'game' for session s-1 is unavailable: the client does \
             not implement mcp/message"
        );
    }
}
