//! MCP transport abstraction — HTTP (Streamable HTTP) and stdio.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Callback invoked when the server sends a JSON-RPC request (with `id` and
/// `method`) back to the client during an ongoing request.  The callback
/// returns the JSON-RPC response object to send back.
pub type ServerRequestHandler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

/// Callback invoked when the server sends a JSON-RPC notification (no `id`,
/// has `method`).  Used to detect `notifications/tools/list_changed` etc.
pub type NotificationHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// The server does not know the session id we sent it.
///
/// A Streamable HTTP server may keep per-session state, and the MCP spec has
/// it answer `404` for any session id it no longer knows. Servers built on
/// `rmcp` drop a session after five minutes of silence, so an agent that goes
/// a quiet afternoon without touching one comes back holding an id that names
/// nothing.
///
/// The transport cannot fix this by itself — the handshake that mints a
/// session lives a layer up — so it clears the dead id and reports *this*
/// rather than a message. `McpClient::send` matches on it, re-initializes,
/// and replays the request once.
#[derive(Debug)]
pub struct SessionExpired {
    /// The server that rejected the id, for the message.
    pub url: String,
    /// What the server said, if anything.
    pub body: String,
}

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP session at {} is no longer known to the server: {}",
            self.url, self.body
        )
    }
}

impl std::error::Error for SessionExpired {}

/// Render a response body for a one-line error message.
///
/// The bodies that reach here are short by convention (`Not Found: Session
/// not found`), but nothing enforces that: a reverse proxy in front of the
/// server answers its own errors, and those are full HTML pages. Cap the
/// length so one misrouted request cannot flood the log.
fn describe_body(body: &str) -> String {
    const MAX: usize = 200;
    let body = body.trim();
    if body.is_empty() {
        return "(empty body)".to_string();
    }
    match body.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_string(),
    }
}

#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and receive the final response.
    ///
    /// For HTTP, the response may arrive as a single JSON body **or** as an
    /// SSE stream containing interleaved server-initiated requests.  For stdio
    /// the response arrives as newline-delimited JSON on stdout.
    ///
    /// `on_server_request` is called for any server-initiated request that
    /// arrives before the final response.  The returned value is sent back to
    /// the server.
    ///
    /// `on_notification` is called for any notification (no `id`, has `method`).
    async fn request(
        &self,
        body: &Value,
        on_server_request: &ServerRequestHandler,
        on_notification: &NotificationHandler,
    ) -> Result<Value>;

    /// Send a JSON-RPC notification (no `id`, no response expected).
    ///
    /// Notifications must not go through [`request`], which blocks waiting
    /// for a matching response that never arrives — over stdio that hangs
    /// the whole startup path.
    async fn notify(&self, body: &Value) -> Result<()>;

    /// Gracefully shut down the transport (close connection / kill child).
    async fn shutdown(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// HTTP (Streamable HTTP) transport
// ---------------------------------------------------------------------------

pub struct HttpTransport {
    url: String,
    api_key: Option<String>,
    http: reqwest::Client,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(url: String, api_key: Option<String>) -> Self {
        Self {
            url,
            api_key,
            http: reqwest::Client::new(),
            session_id: Mutex::new(None),
        }
    }

    /// A POST to the endpoint carrying the headers every message needs.
    ///
    /// Every outbound message goes through here, including the responses to
    /// server-initiated requests: those are POSTs to the same authenticated
    /// endpoint, so leaving the bearer off makes the server answer `401` to a
    /// reply it is blocking on.
    fn post(&self, session_id: &Option<String>) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");

        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        if let Some(sid) = session_id {
            req = req.header("mcp-session-id", sid.as_str());
        }
        req
    }

    /// Build a POST request with standard headers.
    fn build_request(&self, body: &Value, session_id: &Option<String>) -> reqwest::RequestBuilder {
        self.post(session_id).json(body)
    }

    /// Send a JSON-RPC response back to the server (for server-initiated requests).
    async fn send_response(&self, response: &Value, session_id: &Option<String>) -> Result<()> {
        self.post(session_id)
            .json(response)
            .send()
            .await
            .context("Failed to send response to MCP server")?;
        Ok(())
    }

    /// Turn a non-2xx response into what the caller should see.
    ///
    /// `Ok` is a real outcome here: some servers put a JSON-RPC error object
    /// on a 4xx (rmcp answers `400` that way for a malformed request), and
    /// that object is the server answering the request. Its `message` is what
    /// the caller wants, not the status line — so it is handed back as a
    /// response and reported through the same path as any other JSON-RPC
    /// error.
    ///
    /// Everything else becomes an error naming the status and the body. That
    /// matters more than it looks: these bodies are plain text, and the code
    /// that used to run here fed them straight to a JSON parser, so a `404`,
    /// a `401` and a proxy's error page all surfaced as
    /// `expected value at line 1 column 1` with nothing pointing at the
    /// server.
    async fn interpret_error_status(
        &self,
        status: reqwest::StatusCode,
        sent_session_id: bool,
        resp: reqwest::Response,
    ) -> Result<Value> {
        let body = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::NOT_FOUND && sent_session_id {
            // Drop the dead id here rather than at the retry, so that a
            // caller which gives up still leaves the transport able to
            // handshake on its next use.
            *self.session_id.lock().await = None;
            return Err(SessionExpired {
                url: self.url.clone(),
                body: describe_body(&body),
            }
            .into());
        }

        if let Ok(value) = serde_json::from_str::<Value>(&body)
            && value.get("error").is_some()
        {
            return Ok(value);
        }

        bail!(
            "MCP server returned HTTP {status}: {}",
            describe_body(&body)
        )
    }

    /// Parse an SSE `data:` line into JSON.
    fn parse_sse_data(raw: &str) -> Option<Value> {
        let data_line = raw.lines().find(|l| l.starts_with("data:"))?;
        let data = data_line.strip_prefix("data:").unwrap_or("").trim();
        serde_json::from_str(data).ok()
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(
        &self,
        body: &Value,
        on_server_request: &ServerRequestHandler,
        on_notification: &NotificationHandler,
    ) -> Result<Value> {
        let session_id = self.session_id.lock().await.clone();
        let resp = self
            .build_request(body, &session_id)
            .send()
            .await
            .context("Failed to send request to MCP server")?;

        // Capture session id from response header.
        if let Some(sid) = resp.headers().get("mcp-session-id")
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().await = Some(s.to_string());
        }

        // Before anything reads the body: a failing status has a body that is
        // not a JSON-RPC message, and parsing it as one hides the status.
        let status = resp.status();
        if !status.is_success() {
            return self
                .interpret_error_status(status, session_id.is_some(), resp)
                .await;
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE stream — read events until we get the final result.
            let current_sid = self.session_id.lock().await.clone();
            let req_id = body.get("id").cloned().unwrap_or(Value::Null);
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();

            loop {
                match stream.next().await {
                    None => bail!("SSE stream ended without a final result"),
                    Some(Err(e)) => bail!("SSE stream error: {e}"),
                    Some(Ok(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = buf.find("\n\n") {
                            let raw = buf[..pos].to_string();
                            buf.drain(..pos + 2);

                            let Some(data) = Self::parse_sse_data(&raw) else {
                                continue;
                            };

                            // Server-initiated request: has both `id` and `method`.
                            if data.get("method").is_some()
                                && data.get("id").is_some()
                                && data.get("result").is_none()
                            {
                                let method = data["method"].as_str().unwrap_or("");
                                let params = data.get("params").cloned().unwrap_or(Value::Null);
                                let mut response = on_server_request(method, &params);
                                // Attach the request id.
                                if let Value::Object(ref mut map) = response {
                                    map.insert("id".to_string(), data["id"].clone());
                                    map.entry("jsonrpc".to_string())
                                        .or_insert_with(|| json!("2.0"));
                                }
                                if let Err(e) = self.send_response(&response, &current_sid).await {
                                    warn!("Failed to send server-request response: {e}");
                                }
                                continue;
                            }

                            // Final result or error for our request.
                            if data.get("id") == Some(&req_id)
                                && (data.get("result").is_some() || data.get("error").is_some())
                            {
                                return Ok(data);
                            }

                            // Notification (no id) — dispatch to handler.
                            if let Some(method) = data.get("method").and_then(|m| m.as_str()) {
                                let params = data.get("params").cloned().unwrap_or(Value::Null);
                                on_notification(method, &params);
                            } else {
                                debug!("SSE notification (unrecognized): {data}");
                            }
                        }
                    }
                }
            }
        } else {
            // Plain JSON response.
            let body = resp
                .text()
                .await
                .context("Failed to read the MCP response body")?;
            serde_json::from_str(&body).with_context(|| {
                format!(
                    "MCP server answered {status} as `{content_type}`, which is neither SSE nor \
                     JSON: {}",
                    describe_body(&body)
                )
            })
        }
    }

    async fn notify(&self, body: &Value) -> Result<()> {
        let session_id = self.session_id.lock().await.clone();
        let resp = self
            .build_request(body, &session_id)
            .send()
            .await
            .context("Failed to send notification to MCP server")?;

        // A notification has no reply to parse, but it still has a status,
        // and dropping it meant a rejected `notifications/initialized` looked
        // exactly like an accepted one.
        let status = resp.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
                *self.session_id.lock().await = None;
            }
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "MCP server rejected a notification with HTTP {status}: {}",
                describe_body(&body)
            );
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // HTTP transport is stateless per-request; nothing to shut down.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

pub struct StdioTransport {
    stdin: Mutex<tokio::process::ChildStdin>,
    reader: Mutex<BufReader<tokio::process::ChildStdout>>,
    child: Mutex<tokio::process::Child>,
}

impl StdioTransport {
    pub async fn new(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server process: {command}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdin of child process"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdout of child process"))?;

        Ok(Self {
            stdin: Mutex::new(stdin),
            reader: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(child),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(
        &self,
        body: &Value,
        on_server_request: &ServerRequestHandler,
        on_notification: &NotificationHandler,
    ) -> Result<Value> {
        let req_id = body.get("id").cloned().unwrap_or(Value::Null);

        // Write the request as a single JSON line to stdin.
        {
            let mut stdin = self.stdin.lock().await;
            let mut line = serde_json::to_string(body)?;
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .await
                .context("Failed to write to MCP server stdin")?;
            stdin.flush().await?;
        }

        // Read lines from stdout until we get the matching response.
        let mut reader = self.reader.lock().await;
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .context("Failed to read from MCP server stdout")?;
            if n == 0 {
                bail!("MCP server process closed stdout unexpectedly");
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let data: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    debug!("Ignoring non-JSON line from MCP server: {e}");
                    continue;
                }
            };

            // Server-initiated request: has both `id` and `method`.
            if data.get("method").is_some()
                && data.get("id").is_some()
                && data.get("result").is_none()
            {
                let method = data["method"].as_str().unwrap_or("");
                let params = data.get("params").cloned().unwrap_or(Value::Null);
                let mut response = on_server_request(method, &params);
                if let Value::Object(ref mut map) = response {
                    map.insert("id".to_string(), data["id"].clone());
                    map.entry("jsonrpc".to_string())
                        .or_insert_with(|| json!("2.0"));
                }
                // Send response back via stdin.
                let mut stdin = self.stdin.lock().await;
                let mut resp_line = serde_json::to_string(&response)?;
                resp_line.push('\n');
                stdin.write_all(resp_line.as_bytes()).await?;
                stdin.flush().await?;
                continue;
            }

            // Final result or error for our request.
            if data.get("id") == Some(&req_id)
                && (data.get("result").is_some() || data.get("error").is_some())
            {
                return Ok(data);
            }

            // Notification — dispatch to handler.
            if let Some(method) = data.get("method").and_then(|m| m.as_str()) {
                let params = data.get("params").cloned().unwrap_or(Value::Null);
                on_notification(method, &params);
            } else {
                debug!("stdio message (unrecognized): {data}");
            }
        }
    }

    async fn notify(&self, body: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let mut line = serde_json::to_string(body)?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .context("Failed to write notification to MCP server stdin")?;
        stdin.flush().await?;
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // Drop stdin to signal the child, then kill it.
        drop(self.stdin.lock().await);
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A failing HTTP status used to be indistinguishable from a malformed
/// answer: the transport skipped straight to `resp.json()`, so `404`, `401`
/// and a proxy's HTML error page all surfaced as
/// `Failed to parse JSON response: expected value at line 1 column 1`. These
/// tests pin the status reaching the caller, and the one status that is
/// recoverable actually being recovered from.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpServerConfig, McpTransportConfig, McpTrust};
    use crate::mcp_client::McpClient;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Bind a loopback port, serve `router` on it, and return the `/mcp` URL.
    ///
    /// The task is left running: every test here finishes in milliseconds and
    /// the port dies with the test binary.
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

    /// Answer every POST with one canned response.
    async fn serving(status: StatusCode, content_type: &'static str, body: &'static str) -> String {
        let router = Router::new().route(
            "/mcp",
            post(move || async move {
                (status, [("content-type", content_type)], body).into_response()
            }),
        );
        serve(router).await
    }

    /// The two callbacks `request` takes, as the trait objects it wants.
    /// Nothing under test here calls them; the transport-level tests drive
    /// plain request/response exchanges.
    fn handlers() -> (ServerRequestHandler, NotificationHandler) {
        let on_request: ServerRequestHandler = Arc::new(|_: &str, _: &Value| Value::Null);
        let on_notification: NotificationHandler = Arc::new(|_: &str, _: &Value| {});
        (on_request, on_notification)
    }

    fn http_client(url: String, api_key: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "ledger".to_string(),
            trust: McpTrust::Read,
            transport: McpTransportConfig::Http {
                url,
                api_key: api_key.map(str::to_string),
            },
        }
    }

    /// Every request the stub saw, as `(method, session id sent)`.
    type Seen = Arc<StdMutex<Vec<(String, Option<String>)>>>;

    /// A stub MCP server that hands out sessions and forgets the first one.
    ///
    /// Forgetting happens the moment the first handshake completes, which is
    /// the same sequence a real server produces after five idle minutes —
    /// without the five minutes.
    #[derive(Clone, Default)]
    struct Stub {
        seen: Seen,
        /// The `authorization` header of every request, in arrival order.
        auth: Arc<StdMutex<Vec<Option<String>>>>,
        /// Sessions the server still knows.
        live: Arc<StdMutex<HashSet<String>>>,
        minted: Arc<AtomicUsize>,
    }

    impl Stub {
        fn router(&self) -> Router {
            Router::new()
                .route("/mcp", post(Self::handle))
                .with_state(self.clone())
        }

        fn methods(&self) -> Vec<String> {
            self.seen
                .lock()
                .expect("seen")
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }

        fn sessions(&self) -> Vec<Option<String>> {
            self.seen
                .lock()
                .expect("seen")
                .iter()
                .map(|(_, s)| s.clone())
                .collect()
        }

        async fn handle(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
            let header = |name: &str| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let method = message
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let session = header("mcp-session-id");

            stub.seen
                .lock()
                .expect("seen")
                .push((method.clone(), session.clone()));
            stub.auth
                .lock()
                .expect("auth")
                .push(header("authorization"));

            if method == "initialize" {
                let n = stub.minted.fetch_add(1, Ordering::Relaxed) + 1;
                let id = format!("s{n}");
                stub.live.lock().expect("live").insert(id.clone());
                let result = json!({
                    "jsonrpc": "2.0",
                    "id": message.get("id").cloned().unwrap_or(Value::Null),
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": {"name": "stub", "version": "0"}
                    }
                });
                return (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", id.as_str()),
                    ],
                    result.to_string(),
                )
                    .into_response();
            }

            // Anything else needs a session the server still knows. This is
            // the branch that produced the original bug report.
            let known = session
                .as_ref()
                .is_some_and(|s| stub.live.lock().expect("live").contains(s));
            if !known {
                return (StatusCode::NOT_FOUND, "Not Found: Session not found").into_response();
            }

            if method.starts_with("notifications/") {
                // The first session goes idle the instant its handshake ends.
                if session.as_deref() == Some("s1") {
                    stub.live.lock().expect("live").remove("s1");
                }
                return StatusCode::ACCEPTED.into_response();
            }

            let result = json!({
                "jsonrpc": "2.0",
                "id": message.get("id").cloned().unwrap_or(Value::Null),
                "result": {"content": [{"type": "text", "text": "[]"}]}
            });
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                result.to_string(),
            )
                .into_response()
        }
    }

    /// The reported failure, end to end: the handshake succeeds, the session
    /// goes away, and the next tool call must still return a result rather
    /// than an error that only a restart clears.
    #[tokio::test]
    async fn an_expired_session_is_re_established_and_the_call_retried() {
        let stub = Stub::default();
        let url = serve(stub.router()).await;
        let client = McpClient::new(&http_client(url, Some("secret")), "/tmp")
            .await
            .expect("client");

        client.connect().await.expect("the first handshake");
        let result = client
            .call_tool("list_accounts", &json!({}))
            .await
            .expect("a forgotten session must be re-established, not reported");

        assert_eq!(
            result["content"][0]["text"], "[]",
            "the retry's result is what the caller gets"
        );
        assert_eq!(
            stub.methods(),
            vec![
                "initialize",
                "notifications/initialized",
                "tools/call",
                "initialize",
                "notifications/initialized",
                "tools/call",
            ],
            "the recovery is a full handshake, not a bare replay"
        );
        assert_eq!(
            stub.sessions().last().expect("a last request"),
            &Some("s2".to_string()),
            "the replayed call must carry the new session, not the dead one"
        );
    }

    /// The retry happens once. A server that answers `404` to everything is
    /// not a stale session, and looping on it would bury the real error.
    #[tokio::test]
    async fn a_server_that_always_404s_is_reported_rather_than_retried_forever() {
        let stub = Stub::default();
        // Never mint a live session: every non-initialize request 404s.
        stub.live.lock().expect("live").clear();
        let url = serve(
            Router::new()
                .route(
                    "/mcp",
                    post(|headers: HeaderMap, body: String| async move {
                        let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        if message.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                            let _ = headers;
                            return (
                                StatusCode::OK,
                                [
                                    ("content-type", "application/json"),
                                    ("mcp-session-id", "s1"),
                                ],
                                json!({"jsonrpc":"2.0","id":1,"result":{}}).to_string(),
                            )
                                .into_response();
                        }
                        (StatusCode::NOT_FOUND, "Not Found: Session not found").into_response()
                    }),
                )
                .with_state(()),
        )
        .await;

        let client = McpClient::new(&http_client(url, None), "/tmp")
            .await
            .expect("client");
        client.connect().await.expect("handshake");

        let err = client
            .call_tool("list_accounts", &json!({}))
            .await
            .expect_err("a permanently unknown session must surface");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("expected value at line 1 column 1"),
            "the JSON parser must never be what reports an HTTP status, got: {msg}"
        );
    }

    /// `401` is what an unauthenticated request gets, and its body is empty.
    /// The status is the entire diagnosis, so it has to reach the caller.
    #[tokio::test]
    async fn an_error_status_names_itself_instead_of_a_parse_failure() {
        let url = serving(StatusCode::UNAUTHORIZED, "text/plain", "").await;
        let transport = HttpTransport::new(url, None);
        let (on_request, on_notification) = handlers();

        let err = transport
            .request(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                &on_request,
                &on_notification,
            )
            .await
            .expect_err("401 is not a response");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("401"),
            "the status must be in the message: {msg}"
        );
        assert!(
            msg.contains("(empty body)"),
            "an empty body must say so rather than read as a parse failure: {msg}"
        );
    }

    /// `rmcp` answers a malformed request with `400` and a JSON-RPC error
    /// object. That object is the server answering, and its `message` beats
    /// the status line — so it must not be swallowed by the status check.
    #[tokio::test]
    async fn a_json_rpc_error_carried_on_a_4xx_still_reaches_the_caller() {
        let url = serving(
            StatusCode::BAD_REQUEST,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"unsupported protocol version"}}"#,
        )
        .await;
        let client = McpClient::new(&http_client(url, None), "/tmp")
            .await
            .expect("client");

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

    /// The SSE path is the one every stateful server uses; the status check
    /// runs before it and must not have swallowed it.
    #[tokio::test]
    async fn an_sse_response_is_still_parsed() {
        let url = serving(
            StatusCode::OK,
            "text/event-stream",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        )
        .await;
        let transport = HttpTransport::new(url, None);
        let (on_request, on_notification) = handlers();

        let response = transport
            .request(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                &on_request,
                &on_notification,
            )
            .await
            .expect("an SSE answer is an answer");
        assert_eq!(response["result"]["ok"], true);
    }

    /// A reply to a server-initiated request is a POST to the same guarded
    /// endpoint. Without the bearer the server answers `401` to a message it
    /// is blocking on, and the original request hangs until it times out.
    #[tokio::test]
    async fn a_reply_to_a_server_initiated_request_carries_the_bearer() {
        let stub = Stub::default();
        let url = serve(stub.router()).await;
        let transport = HttpTransport::new(url, Some("secret".to_string()));

        transport
            .send_response(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}), &None)
            .await
            .expect("the reply is sent");

        assert_eq!(
            stub.auth.lock().expect("auth").first(),
            Some(&Some("Bearer secret".to_string())),
            "the reply must authenticate exactly as the request did"
        );
    }
}
