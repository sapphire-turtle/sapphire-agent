//! An rmcp transport that carries MCP over ACP's `mcp/message`.
//!
//! An ACP client can hand the agent MCP servers it hosts itself
//! (`"type": "acp"` in `mcpServers`, the MCP-over-ACP RFD). There is no
//! socket or process to reach them by: every MCP request travels as one
//! ACP `mcp/message` request on the client's connection, its inner result or
//! error comes back as that request's outer response, and the only traffic
//! the client sends unasked is notifications scoped to a request still in
//! flight. Cancelling is the outer `$/cancel_request`.
//!
//! That is an envelope, not a byte stream, so this is not a framed
//! transport: [`AcpMcpTransport`] takes each JSON-RPC message rmcp sends,
//! turns a request into one [`McpOverAcpPeer::request`] in its own task, and
//! queues the answer (and any notifications for it) back as the JSON-RPC
//! messages rmcp expects to receive. The ACP side is seen only through
//! [`McpOverAcpPeer`], as JSON, so this file knows nothing of ACP's types.

use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::debug;

/// How long one `mcp/message` may take before the agent gives up on it.
/// `subscriptions/listen` is exempt: it is meant to stay open.
pub const ACP_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The long-lived request a 2026-07-28 server pushes notifications on. It
/// ends when the peer closes or rmcp cancels it, never by timeout.
const SUBSCRIPTIONS_LISTEN: &str = "subscriptions/listen";

/// The `_meta` key rmcp routes subscription notifications (and the listen
/// result) by: the id of the listen request they belong to.
const SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";

/// How often [`AcpMcpTransport::receive`] looks for a closed peer while
/// nothing arrives. [`McpOverAcpPeer`] can only be asked, not awaited, so
/// this bounds how late rmcp learns that an idle connection went away.
const CLOSED_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// JSON-RPC's internal error, for failures of the envelope rather than of
/// the MCP server: an outer ACP error, a closed connection, a timeout.
const INTERNAL_ERROR: i64 = -32603;

/// One MCP-over-ACP binding: the ACP connection, seen only as JSON.
#[async_trait::async_trait]
pub trait McpOverAcpPeer: Send + Sync + 'static {
    /// Send one `mcp/message` request and wait for its outer response.
    /// `Ok(McpCarrier)` is the successful outer response; `Err` is an outer
    /// ACP error or a closed connection. Dropping the future cancels the
    /// outer request (`$/cancel_request`). Implementations must resolve
    /// every request in flight with `Err(McpOverAcpError::Closed)` promptly
    /// when the connection closes, rather than leave it to the timeout.
    async fn request(
        &self,
        server_id: &str,
        request_id: &str,
        method: &str,
        params: Value,
    ) -> Result<McpCarrier, McpOverAcpError>;
    /// Notifications the client sends for `request_id` while it is active,
    /// as `(method, params)`.
    fn notifications(
        &self,
        server_id: &str,
        request_id: &str,
    ) -> mpsc::UnboundedReceiver<(String, Value)>;
    fn is_closed(&self) -> bool;
}

/// What a successful outer `mcp/message` response carries: the inner MCP
/// result, or the inner MCP error (`-33001` server unavailable, `-33002`
/// backend failed, or whatever the MCP server itself answered).
#[derive(Debug, Clone, PartialEq)]
pub enum McpCarrier {
    Result(Value),
    Error {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

/// Why an `mcp/message` produced no outer response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOverAcpError {
    Closed,
    Outer { code: i64, message: String },
    Timeout(Duration),
}

impl fmt::Display for McpOverAcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "ACP connection closed"),
            Self::Outer { code, message } => write!(f, "ACP error {code}: {message}"),
            Self::Timeout(after) => write!(f, "mcp/message timed out after {after:?}"),
        }
    }
}

impl std::error::Error for McpOverAcpError {}

/// The in-flight requests' tasks, keyed by rmcp's JSON-RPC id (as its JSON
/// text, since `Value` is not `Hash`).
type Pending = Arc<Mutex<HashMap<String, AbortHandle>>>;

/// An rmcp client transport over one MCP server an ACP client provides.
pub struct AcpMcpTransport {
    peer: Arc<dyn McpOverAcpPeer>,
    server_id: Arc<str>,
    inbox_tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
    inbox: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
    pending: Pending,
}

impl AcpMcpTransport {
    pub fn new(peer: Arc<dyn McpOverAcpPeer>, server_id: impl Into<String>) -> Self {
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        Self {
            peer,
            server_id: server_id.into().into(),
            inbox_tx,
            inbox,
            pending: Arc::default(),
        }
    }

    /// Start one request's round trip in its own task.
    fn start_request(&self, id: Value, method: String, params: Value) {
        let key = id.to_string();
        let request_id = format!("mcp-request:{}", uuid::Uuid::now_v7());
        let peer = self.peer.clone();
        let server_id = self.server_id.clone();
        let inbox = self.inbox_tx.clone();
        let pending = self.pending.clone();
        let task_key = key.clone();
        // Held until the task has registered itself, so a request that
        // finishes at once cannot unregister before it is registered.
        let mut registry = self.pending.lock().unwrap();
        let listen = method == SUBSCRIPTIONS_LISTEN;
        let task = tokio::spawn(async move {
            // The client's MCP server never sees rmcp's id for a listen
            // request (only the outer ACP one), so it cannot tag what it
            // sends on it with that id; tag it here, which is what rmcp
            // routes subscription traffic by.
            let stamp = |mut params: Value| {
                if listen {
                    set_subscription_id(&mut params, &id);
                }
                params
            };
            // Subscribed before the request goes out, so nothing the
            // client says about it can arrive unheard.
            let mut notes = peer.notifications(&server_id, &request_id);
            let round_trip = async {
                let response = peer.request(&server_id, &request_id, &method, params);
                tokio::pin!(response);
                loop {
                    tokio::select! {
                        outcome = &mut response => break outcome,
                        Some((method, params)) = notes.recv() => {
                            deliver_notification(&inbox, &server_id, method, stamp(params));
                        }
                    }
                }
            };
            let mut outcome = if listen {
                round_trip.await
            } else {
                tokio::time::timeout(ACP_MCP_REQUEST_TIMEOUT, round_trip)
                    .await
                    .unwrap_or(Err(McpOverAcpError::Timeout(ACP_MCP_REQUEST_TIMEOUT)))
            };
            if let Ok(McpCarrier::Result(result)) = &mut outcome
                && listen
            {
                set_subscription_id(result, &id);
            }
            // What the client said before answering, rmcp hears first.
            while let Ok((method, params)) = notes.try_recv() {
                deliver_notification(&inbox, &server_id, method, stamp(params));
            }
            let _ = inbox.send(response_message(id, outcome));
            // Only after the answer is queued: `receive` reads an empty
            // registry as "nothing more will arrive".
            pending.lock().unwrap().remove(&task_key);
        });
        registry.insert(key, task.abort_handle());
    }

    /// `notifications/cancelled`: abort the request's task. Dropping its
    /// `request` future is what sends the outer `$/cancel_request`.
    fn cancel_request(&self, params: &Value) {
        let Some(id) = params.get("requestId") else {
            return;
        };
        if let Some(task) = self.pending.lock().unwrap().remove(&id.to_string()) {
            task.abort();
        }
    }
}

/// Set `_meta`'s subscription id in `object` (notification params or a
/// listen result) to rmcp's `id` for the listen request.
fn set_subscription_id(object: &mut Value, id: &Value) {
    if !object.is_object() {
        *object = json!({});
    }
    let meta = object
        .as_object_mut()
        .expect("just made an object")
        .entry("_meta")
        .or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    meta.as_object_mut()
        .expect("just made an object")
        .insert(SUBSCRIPTION_ID_META_KEY.to_string(), id.clone());
}

/// Queue a client notification as the JSON-RPC notification rmcp reads.
fn deliver_notification(
    inbox: &mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
    server_id: &str,
    method: String,
    params: Value,
) {
    let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
    match serde_json::from_value(message) {
        Ok(message) => {
            let _ = inbox.send(message);
        }
        Err(e) => debug!("MCP over ACP '{server_id}': dropped notification {method}: {e}"),
    }
}

/// The JSON-RPC response or error rmcp gets for request `id`.
fn response_message(
    id: Value,
    outcome: Result<McpCarrier, McpOverAcpError>,
) -> RxJsonRpcMessage<RoleClient> {
    let message = match outcome {
        Ok(McpCarrier::Result(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Ok(McpCarrier::Error {
            code,
            message,
            data,
        }) => error_json(&id, code, message, data),
        Err(e) => error_json(&id, INTERNAL_ERROR, e.to_string(), None),
    };
    serde_json::from_value(message).unwrap_or_else(|e| {
        // A result rmcp cannot read is the provider's fault, and still the
        // answer to this request: fail it rather than leave it hanging.
        serde_json::from_value(error_json(
            &id,
            INTERNAL_ERROR,
            format!("unreadable MCP response: {e}"),
            None,
        ))
        .expect("a JSON-RPC error with a valid id always parses")
    })
}

fn error_json(id: &Value, code: i64, message: String, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

impl rmcp::transport::Transport<RoleClient> for AcpMcpTransport {
    type Error = McpOverAcpError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Everything happens here, synchronously: rmcp awaits `send` before
        // it reads the answer, so the round trip itself must not be in it.
        let outcome = if self.peer.is_closed() {
            Err(McpOverAcpError::Closed)
        } else {
            match serde_json::to_value(&item) {
                Ok(mut message) => {
                    let method = message
                        .get("method")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let params = message
                        .get_mut("params")
                        .map(Value::take)
                        .unwrap_or_else(|| json!({}));
                    match (message.get_mut("id").map(Value::take), method) {
                        (Some(id), Some(method)) => self.start_request(id, method, params),
                        (None, Some(method)) if method == "notifications/cancelled" => {
                            self.cancel_request(&params)
                        }
                        (None, Some(method)) => debug!(
                            "MCP over ACP '{}': dropped notification {method}",
                            self.server_id
                        ),
                        // A response: there are no provider-originated
                        // requests for it to answer.
                        _ => debug!(
                            "MCP over ACP '{}': dropped a response with nothing to answer",
                            self.server_id
                        ),
                    }
                    Ok(())
                }
                Err(e) => {
                    debug!(
                        "MCP over ACP '{}': unserializable message: {e}",
                        self.server_id
                    );
                    Ok(())
                }
            }
        };
        std::future::ready(outcome)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            if self.peer.is_closed() && self.pending.lock().unwrap().is_empty() {
                // Every task queues its answer before it unregisters, so
                // whatever is left is already in the inbox.
                return self.inbox.try_recv().ok();
            }
            tokio::select! {
                message = self.inbox.recv() => return message,
                _ = tokio::time::sleep(CLOSED_POLL_INTERVAL) => {}
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        for (_, task) in self.pending.lock().unwrap().drain() {
            task.abort();
        }
        self.inbox.close();
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use rmcp::model::{DiscoverResult, ProtocolVersion, ServerCapabilities};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    /// How the fake answers one inner method.
    #[derive(Clone)]
    pub(crate) enum FakeReply {
        /// A successful outer response carrying this.
        Carrier(McpCarrier),
        /// An outer ACP error.
        Outer { code: i64, message: String },
        /// Wait for the `Notify` (or the peer closing), then answer.
        Held(Arc<Notify>, McpCarrier),
        /// Never answer; only closing the peer ends it.
        Hang,
    }

    #[derive(Default)]
    struct State {
        replies: HashMap<String, FakeReply>,
        /// `(server_id, method)` of every request, in order.
        seen: Vec<(String, String)>,
        /// `(method, params)` of every request, in order.
        params: Vec<(String, Value)>,
        /// `(method, request_id)` of every request, in order.
        request_ids: Vec<(String, String)>,
        /// Request ids whose `request` future was dropped unfinished.
        dropped: Vec<String>,
        notifiers: HashMap<String, mpsc::UnboundedSender<(String, Value)>>,
    }

    /// A scripted [`McpOverAcpPeer`]: answers per inner method, records what
    /// it was asked, and can be closed or fed notifications by a test.
    pub(crate) struct FakePeer {
        state: Mutex<State>,
        closed: CancellationToken,
    }

    impl FakePeer {
        /// A peer that answers `server/discover` as a 2026-07-28 server
        /// with tools, and `tools/list` with one tool, `poke`.
        pub(crate) fn new() -> Arc<Self> {
            let discover = DiscoverResult::new(
                vec![ProtocolVersion::LATEST],
                ServerCapabilities::builder().enable_tools().build(),
            );
            let peer = Arc::new(Self {
                state: Mutex::new(State::default()),
                closed: CancellationToken::new(),
            });
            peer.script(
                "server/discover",
                FakeReply::Carrier(McpCarrier::Result(serde_json::to_value(discover).unwrap())),
            );
            peer.script(
                "tools/list",
                FakeReply::Carrier(McpCarrier::Result(json!({
                    "tools": [{
                        "name": "poke",
                        "inputSchema": {"type": "object"},
                        "annotations": {"readOnlyHint": true}
                    }]
                }))),
            );
            peer
        }

        /// Answer `method` with `reply` from now on.
        pub(crate) fn script(&self, method: &str, reply: FakeReply) {
            self.state
                .lock()
                .unwrap()
                .replies
                .insert(method.to_string(), reply);
        }

        /// `(server_id, method)` of every request so far.
        pub(crate) fn seen(&self) -> Vec<(String, String)> {
            self.state.lock().unwrap().seen.clone()
        }

        /// `(method, params)` of every request so far.
        pub(crate) fn params(&self) -> Vec<(String, Value)> {
            self.state.lock().unwrap().params.clone()
        }

        /// Request ids whose future was dropped before it finished.
        pub(crate) fn dropped(&self) -> Vec<String> {
            self.state.lock().unwrap().dropped.clone()
        }

        /// The `requestId` of the latest request for `method`, once one
        /// has been made.
        pub(crate) async fn wait_for_request(&self, method: &str) -> String {
            loop {
                let found = self
                    .state
                    .lock()
                    .unwrap()
                    .request_ids
                    .iter()
                    .rev()
                    .find(|(m, _)| m == method)
                    .map(|(_, id)| id.clone());
                if let Some(id) = found {
                    return id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// Send a client notification scoped to `request_id`. False when
        /// nothing listens for that id.
        pub(crate) fn notify(&self, request_id: &str, method: &str, params: Value) -> bool {
            self.state
                .lock()
                .unwrap()
                .notifiers
                .get(request_id)
                .is_some_and(|tx| tx.send((method.to_string(), params)).is_ok())
        }

        /// Close the connection: pending requests fail with `Closed`.
        pub(crate) fn close(&self) {
            self.closed.cancel();
        }
    }

    /// Records its request id as dropped unless disarmed first.
    struct DropProbe<'a> {
        peer: &'a FakePeer,
        request_id: Option<String>,
    }

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            if let Some(id) = self.request_id.take() {
                self.peer.state.lock().unwrap().dropped.push(id);
            }
        }
    }

    #[async_trait::async_trait]
    impl McpOverAcpPeer for FakePeer {
        async fn request(
            &self,
            server_id: &str,
            request_id: &str,
            method: &str,
            params: Value,
        ) -> Result<McpCarrier, McpOverAcpError> {
            if self.closed.is_cancelled() {
                return Err(McpOverAcpError::Closed);
            }
            let reply = {
                let mut state = self.state.lock().unwrap();
                state.seen.push((server_id.to_string(), method.to_string()));
                state.params.push((method.to_string(), params));
                state
                    .request_ids
                    .push((method.to_string(), request_id.to_string()));
                state.replies.get(method).cloned()
            };
            let mut probe = DropProbe {
                peer: self,
                request_id: Some(request_id.to_string()),
            };
            let outcome = match reply {
                None => Ok(McpCarrier::Error {
                    code: -32601,
                    message: format!("method not found: {method}"),
                    data: None,
                }),
                Some(FakeReply::Carrier(carrier)) => Ok(carrier),
                Some(FakeReply::Outer { code, message }) => {
                    Err(McpOverAcpError::Outer { code, message })
                }
                Some(FakeReply::Held(gate, carrier)) => tokio::select! {
                    _ = gate.notified() => Ok(carrier),
                    _ = self.closed.cancelled() => Err(McpOverAcpError::Closed),
                },
                Some(FakeReply::Hang) => {
                    self.closed.cancelled().await;
                    Err(McpOverAcpError::Closed)
                }
            };
            probe.request_id = None;
            outcome
        }

        fn notifications(
            &self,
            _server_id: &str,
            request_id: &str,
        ) -> mpsc::UnboundedReceiver<(String, Value)> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.state
                .lock()
                .unwrap()
                .notifiers
                .insert(request_id.to_string(), tx);
            rx
        }

        fn is_closed(&self) -> bool {
            self.closed.is_cancelled()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ProgressNotificationParam,
        ProtocolVersion,
    };
    use rmcp::service::{
        ClientLifecycleMode, ClientServiceExt, NotificationContext, RunningService,
    };
    use rmcp::{ClientHandler, ServiceError};
    use std::sync::Mutex;
    use tokio::sync::Notify;

    /// A client handler that only remembers the progress it was told of.
    #[derive(Clone, Default)]
    struct ProgressLog(Arc<Mutex<Vec<ProgressNotificationParam>>>);

    impl ClientHandler for ProgressLog {
        async fn on_progress(
            &self,
            params: ProgressNotificationParam,
            _context: NotificationContext<RoleClient>,
        ) {
            self.0.lock().unwrap().push(params);
        }
    }

    async fn connect<H: ClientHandler>(
        handler: H,
        peer: &Arc<FakePeer>,
    ) -> RunningService<RoleClient, H> {
        handler
            .serve_with_lifecycle(
                AcpMcpTransport::new(peer.clone(), "game:1"),
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::LATEST],
                },
            )
            .await
            .unwrap()
    }

    fn poke() -> CallToolRequestParams {
        CallToolRequestParams::new("poke")
    }

    fn poked() -> McpCarrier {
        let result = CallToolResult::success(vec![ContentBlock::text("poked")]);
        McpCarrier::Result(serde_json::to_value(result).unwrap())
    }

    fn error_code(error: ServiceError) -> (i32, String) {
        match error {
            ServiceError::McpError(e) => (e.code.0, e.message.into_owned()),
            other => panic!("expected an MCP error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rmcp_discovers_and_lists_tools_over_mcp_message() {
        let peer = FakePeer::new();
        let svc = ()
            .serve_with_lifecycle(
                AcpMcpTransport::new(peer.clone(), "game:1"),
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::LATEST],
                },
            )
            .await
            .unwrap();
        let tools = svc.list_all_tools().await.unwrap();
        assert_eq!(tools[0].name, "poke");
        assert!(
            peer.seen()
                .iter()
                .any(|(sid, m)| sid == "game:1" && m == "tools/list")
        );
    }

    #[tokio::test]
    async fn carrier_error_becomes_an_mcp_error() {
        let peer = FakePeer::new();
        peer.script(
            "tools/call",
            FakeReply::Carrier(McpCarrier::Error {
                code: -33002,
                message: "backend failed".into(),
                data: None,
            }),
        );
        let svc = connect((), &peer).await;
        let (code, _) = error_code(svc.call_tool(poke()).await.unwrap_err());
        assert_eq!(code, -33002);
    }

    #[tokio::test]
    async fn outer_error_becomes_an_internal_error() {
        let peer = FakePeer::new();
        peer.script(
            "tools/call",
            FakeReply::Outer {
                code: -32601,
                message: "no mcp/message here".into(),
            },
        );
        let svc = connect((), &peer).await;
        let (code, message) = error_code(svc.call_tool(poke()).await.unwrap_err());
        assert_eq!(code, -32603);
        assert!(message.contains("-32601"), "{message}");
    }

    #[tokio::test]
    async fn notifications_are_routed_by_request_id() {
        let peer = FakePeer::new();
        let gate = Arc::new(Notify::new());
        peer.script("tools/call", FakeReply::Held(gate.clone(), poked()));
        let log = ProgressLog::default();
        let svc = connect(log.clone(), &peer).await;
        // Finished, so nothing listens for it any more.
        let stale_id = peer.wait_for_request("server/discover").await;

        let drive = async {
            let id = peer.wait_for_request("tools/call").await;
            let token = peer
                .params()
                .iter()
                .rev()
                .find(|(m, _)| m == "tools/call")
                .and_then(|(_, p)| p.pointer("/_meta/progressToken").cloned())
                .unwrap_or_else(|| serde_json::json!("t"));
            let progress =
                |n: u32| serde_json::json!({"progressToken": token, "progress": n, "total": 2});
            // Whether or not its task has let go of the channel yet,
            // nothing reads it: only the call's own progress arrives.
            peer.notify(&stale_id, "notifications/progress", progress(1));
            assert!(peer.notify(&id, "notifications/progress", progress(2)));
            tokio::time::timeout(Duration::from_secs(5), async {
                while log.0.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("progress for the call reaches rmcp");
            gate.notify_one();
        };
        let (result, ()) = tokio::join!(svc.call_tool(poke()), drive);
        result.unwrap();
        let progress: Vec<f64> = log.0.lock().unwrap().iter().map(|p| p.progress).collect();
        assert_eq!(progress, vec![2.0]);
    }

    #[tokio::test]
    async fn cancel_aborts_the_outer_request() {
        let peer = FakePeer::new();
        peer.script("tools/call", FakeReply::Hang);
        let svc = connect((), &peer).await;
        let id = tokio::select! {
            _ = svc.call_tool(poke()) => panic!("the call cannot finish"),
            id = peer.wait_for_request("tools/call") => id,
        };
        // The call's future is gone; rmcp sends `notifications/cancelled`.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !peer.dropped().contains(&id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the outer request's future is dropped");
    }

    #[tokio::test]
    async fn pending_requests_fail_when_the_peer_closes() {
        let peer = FakePeer::new();
        peer.script("tools/call", FakeReply::Hang);
        let svc = connect((), &peer).await;
        let close = async {
            peer.wait_for_request("tools/call").await;
            peer.close();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(svc.call_tool(poke()), close)
        })
        .await
        .expect("the call returns once the peer closes");
        assert!(result.is_err());
    }

    /// A listen request is not timed out, and what the client's server
    /// sends on it — knowing only the outer request id — reaches rmcp's
    /// subscription, not the handler, past the per-request timeout.
    #[tokio::test(start_paused = true)]
    async fn a_subscription_is_acknowledged_and_outlives_the_timeout() {
        use rmcp::model::{ServerNotification, SubscriptionFilter};
        let peer = FakePeer::new();
        peer.script("subscriptions/listen", FakeReply::Hang);
        let svc = connect((), &peer).await;
        let acknowledge = async {
            let id = peer.wait_for_request("subscriptions/listen").await;
            assert!(peer.notify(
                &id,
                "notifications/subscriptions/acknowledged",
                serde_json::json!({"notifications": {"toolsListChanged": true}}),
            ));
            id
        };
        let filter = SubscriptionFilter::builder().tools_list_changed().build();
        let listen = tokio::time::timeout(Duration::from_secs(5), svc.peer().listen(filter));
        let (subscription, id) = tokio::join!(listen, acknowledge);
        let mut subscription = subscription
            .expect("the acknowledgment reaches the subscription")
            .expect("the subscription is acknowledged");
        assert_eq!(subscription.acknowledged().tools_list_changed, Some(true));

        tokio::time::sleep(ACP_MCP_REQUEST_TIMEOUT + Duration::from_secs(30)).await;
        assert!(
            !peer.dropped().contains(&id),
            "the listen request was cancelled"
        );
        assert!(peer.notify(
            &id,
            "notifications/tools/list_changed",
            serde_json::json!({}),
        ));
        let next = tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await
            .expect("a notification in time")
            .expect("no subscription error");
        assert!(
            matches!(
                next,
                Some(ServerNotification::ToolListChangedNotification(_))
            ),
            "got {next:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_times_out() {
        let peer = FakePeer::new();
        peer.script("tools/call", FakeReply::Hang);
        let svc = connect((), &peer).await;
        let started = tokio::time::Instant::now();
        let (code, message) = error_code(svc.call_tool(poke()).await.unwrap_err());
        assert_eq!(code, -32603);
        assert_eq!(
            message,
            McpOverAcpError::Timeout(ACP_MCP_REQUEST_TIMEOUT).to_string()
        );
        assert!(started.elapsed() >= ACP_MCP_REQUEST_TIMEOUT);
    }
}
