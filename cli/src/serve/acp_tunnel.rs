//! ACP on stdio: `sapphire-agent acp`, tunnelled to the running agent over
//! the app server's IPC endpoint.
//!
//! An editor starts `sapphire-agent acp` as its agent command. That process
//! is only a relay — like the framework's `mcp` subcommand it dials the
//! server already running on this host and never starts one — and the ACP
//! connection, with the agent loop behind it, runs in the server, in the
//! same [`serve_connection`] that serves `/acp`:
//!
//! ```text
//! editor ──stdio──▶ sapphire-agent acp ──IPC──▶ sapphire-agent serve
//! ```
//!
//! The relay carries ACP lines without reading them. ACP runs both ways —
//! the agent asks the editor for permissions, files and terminals — and a
//! relay that translated methods would need changing with every ACP
//! version. So the methods move one JSON-RPC message (one line) at a time:
//!
//! - [`OPEN`] runs one ACP connection for as long as it lasts. It first
//!   notifies [`OPENED`] with the connection's id, and answers only once
//!   the connection has ended, after a last [`CLOSED`]. The request in
//!   flight is what ties the ACP connection to the IPC one: when the relay
//!   goes away the framework drops its handlers, which closes the ACP
//!   connection (see [`AcpTunnel::open`]).
//! - [`SEND`] `{conn, line}` carries one line from the editor to the agent.
//!   The relay waits for each answer before the next send — the framework
//!   runs every request on its own task, so two sends in flight at once
//!   could arrive in either order.
//! - [`RECV`] `{conn, line}`, a notification, carries one line back.
//! - [`CLOSE`] `{conn}` ends the connection: what the relay sends when the
//!   editor closes its stdin.
//!
//! There is no token on this path. The IPC endpoint admits only the OS user
//! that runs the server, and the room profile a token would have picked is
//! named by `[acp].stdio_room_profile` instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::StreamExt;
use sapphire_framework::ipc::{
    Client, ClientInfo, PeerHandle, RequestCtx, Router, RpcError, codes,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::ServeState;
use super::acp::serve_connection;

/// The version of the agent's own IPC methods — the ones below and
/// [`super::chat_ipc`]'s — apart from the framework's. The handshake refuses
/// a server that speaks another, so a client never talks to an agent that
/// would read its calls differently, or lacks them.
///
/// 2: added `sapphire-agent.chat.*`.
pub const APP_API: u32 = 2;

/// Run one ACP connection; answers once it has ended.
pub const OPEN: &str = "sapphire-agent.acp.open";
/// One line, editor → agent.
pub const SEND: &str = "sapphire-agent.acp.send";
/// End a connection.
pub const CLOSE: &str = "sapphire-agent.acp.close";
/// Notification: the connection [`OPEN`] started, and its id.
pub const OPENED: &str = "sapphire-agent.acp.opened";
/// Notification: one line, agent → editor.
pub const RECV: &str = "sapphire-agent.acp.recv";
/// Notification: the connection has ended; nothing follows it.
pub const CLOSED: &str = "sapphire-agent.acp.closed";

/// The server's side: the methods above, registered on the app server's
/// router.
///
/// The router is built before the agent is — the app server has to be
/// running for the workspace the agent is built around to open — so the
/// [`ServeState`] arrives later, through [`attach`](Self::attach). Until
/// then [`OPEN`] says the agent is still starting.
#[derive(Clone, Default)]
pub struct AcpTunnel {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    state: OnceLock<Arc<ServeState>>,
    conns: Mutex<HashMap<String, Conn>>,
}

/// One open connection, as [`SEND`] and [`CLOSE`] reach it.
struct Conn {
    /// The IPC client that opened it. Another client's sends are refused:
    /// they come from the same OS user, but a line sent into someone else's
    /// conversation is a mistake worth stopping.
    owner: u32,
    lines: tokio::sync::mpsc::Sender<String>,
    cancel: CancellationToken,
}

#[derive(Deserialize)]
struct SendParams {
    conn: String,
    line: String,
}

#[derive(Deserialize)]
struct CloseParams {
    conn: String,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: codes::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

impl AcpTunnel {
    /// Start answering [`OPEN`], with the agent now built.
    pub fn attach(&self, state: Arc<ServeState>) {
        if self.inner.state.set(state).is_err() {
            warn!("ACP tunnel: attached twice; keeping the first agent");
        }
    }

    /// Register the methods on `router`.
    pub fn router(&self, router: Router) -> Router {
        let open = self.clone();
        let send = self.clone();
        let close = self.clone();
        router
            .method(OPEN, move |ctx| {
                let tunnel = open.clone();
                async move { tunnel.open(ctx.peer).await }
            })
            .method(SEND, move |ctx| {
                let tunnel = send.clone();
                async move { tunnel.send(ctx).await }
            })
            .method(CLOSE, move |ctx| {
                let tunnel = close.clone();
                async move { tunnel.close(ctx).await }
            })
    }

    /// Run one ACP connection for `peer` until it ends.
    ///
    /// The connection runs on a task of its own, not in this handler. The
    /// framework drops a client's handlers when the client disconnects, and
    /// dropping [`serve_connection`] part-way would skip its teardown —
    /// which releases the sessions the connection held, and otherwise
    /// leaves every later editor warned about a holder that has gone. So
    /// this handler holds only a guard: dropped with it, the guard cancels
    /// the connection's input, and the task ends the way any connection
    /// whose peer has left does.
    async fn open(&self, peer: PeerHandle) -> std::result::Result<Value, RpcError> {
        let Some(state) = self.inner.state.get().cloned() else {
            return Err(failed(
                "the agent is still starting; try again once `sapphire-agent status` shows it \
                 serving",
            ));
        };
        let Some(profile) = state
            .config
            .acp
            .as_ref()
            .and_then(|a| a.stdio_room_profile.clone())
        else {
            return Err(failed(
                "ACP on stdio needs a room profile: set [acp].stdio_room_profile in this host's \
                 config.toml",
            ));
        };

        let conn = uuid::Uuid::now_v7().to_string();
        let (lines_tx, lines_rx) = tokio::sync::mpsc::channel::<String>(32);
        let cancel = CancellationToken::new();
        self.inner.conns.lock().unwrap().insert(
            conn.clone(),
            Conn {
                owner: peer.client().pid,
                lines: lines_tx,
                cancel: cancel.clone(),
            },
        );

        // Before the task can send anything: the relay learns the id from
        // this, and notifications reach it in the order they were sent.
        if peer.notify(OPENED, json!({ "conn": conn })).await.is_err() {
            self.inner.conns.lock().unwrap().remove(&conn);
            return Err(failed("the client went away"));
        }
        info!(
            "ACP: stdio connection {conn} opened for room profile '{profile}' (pid {})",
            peer.client().pid
        );

        let incoming = tokio_stream::wrappers::ReceiverStream::new(lines_rx)
            .map(Ok)
            .take_until(cancel.clone().cancelled_owned());
        let outgoing = futures_util::sink::unfold(
            (peer.clone(), conn.clone()),
            |(peer, conn), line: String| async move {
                peer.notify(RECV, json!({ "conn": conn, "line": line }))
                    .await
                    .map_err(std::io::Error::other)?;
                Ok::<_, std::io::Error>((peer, conn))
            },
        );

        let task = {
            let inner = Arc::clone(&self.inner);
            let conn = conn.clone();
            tokio::spawn(async move {
                serve_connection(outgoing, incoming, state, profile).await;
                inner.conns.lock().unwrap().remove(&conn);
            })
        };
        let _close_on_drop = cancel.drop_guard();
        if let Err(e) = task.await {
            warn!("ACP: stdio connection {conn} ended abnormally: {e}");
        }
        // Best effort: a relay that has gone needs no word.
        let _ = peer.notify(CLOSED, json!({ "conn": conn })).await;
        Ok(json!({}))
    }

    async fn send(&self, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
        let params: SendParams = serde_json::from_value(ctx.params)
            .map_err(|e| invalid(format!("bad {SEND} parameters: {e}")))?;
        let lines = {
            let conns = self.inner.conns.lock().unwrap();
            let conn = conns
                .get(&params.conn)
                .filter(|c| c.owner == ctx.peer.client().pid)
                .ok_or_else(|| invalid(format!("no ACP connection {}", params.conn)))?;
            conn.lines.clone()
        };
        // Waits while the connection is busy reading, which is the relay's
        // backpressure: it sends nothing more until this is answered.
        lines
            .send(params.line)
            .await
            .map_err(|_| failed(format!("ACP connection {} has closed", params.conn)))?;
        Ok(json!({}))
    }

    async fn close(&self, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
        let params: CloseParams = serde_json::from_value(ctx.params)
            .map_err(|e| invalid(format!("bad {CLOSE} parameters: {e}")))?;
        let conns = self.inner.conns.lock().unwrap();
        // Closing a connection that has already gone is no error: both ends
        // may decide to stop at once.
        if let Some(conn) = conns
            .get(&params.conn)
            .filter(|c| c.owner == ctx.peer.client().pid)
        {
            conn.cancel.cancel();
        }
        Ok(json!({}))
    }
}

/// The client's side: relay `input` (the editor's lines) to the agent over
/// `client`, and the agent's lines to `output`, until either end stops.
///
/// Ends cleanly when the editor closes `input` (after the agent has
/// finished writing) or when the agent ends the connection, and with an
/// error when the server cannot open one or goes away.
pub async fn relay<R, W>(client: Arc<Client>, input: R, output: W) -> Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Subscribed before the open, so its notifications cannot be missed.
    let mut events = client.notifications();
    // Polled in place rather than spawned: the call holds the client, and
    // with it the IPC connection, so it must not outlive this relay.
    let open = client.call::<_, Value>(OPEN, json!({}));
    tokio::pin!(open);

    let conn = tokio::select! {
        opened = wait_opened(&mut events) => opened?,
        answered = &mut open => {
            return Err(match answered {
                Err(e) => anyhow!(e).context("opening an ACP connection"),
                Ok(_) => anyhow!("the agent ended the ACP connection before opening it"),
            });
        }
    };

    // Two hops back: one drains the notifications as fast as they come, so
    // a slow editor never makes the framework's bounded queue drop a line;
    // the other writes them out at the editor's pace. Neither holds the
    // client: both end once it is gone.
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let collect = tokio::spawn(collect_lines(events, conn.clone(), out_tx));
    let write = tokio::spawn(write_lines(out_rx, output));

    let pumped = tokio::select! {
        pumped = pump_input(&client, &conn, input) => pumped,
        // The agent ended the connection first, or the server went away.
        answered = &mut open => {
            return finish(answered, collect, write).await;
        }
    };
    // The editor is done with us: close, then let the agent's last lines
    // out before exiting.
    let _ = client
        .call::<_, Value>(CLOSE, json!({ "conn": conn }))
        .await;
    let finished = finish(open.await, collect, write).await;
    pumped.and(finished)
}

/// `sapphire-agent acp`: relay this process's stdio to the agent running
/// on this host.
pub async fn run_stdio() -> Result<()> {
    let app = crate::APP_CTX.app_name;
    let endpoint = sapphire_framework::ipc::Endpoint::for_app(app)?;
    let info = ClientInfo {
        kind: "acp".to_owned(),
        version: crate::app_server::VERSION.to_owned(),
        api: sapphire_framework_server_api::API_VERSION,
        app_api: Some(APP_API),
        pid: std::process::id(),
    };
    let Some((client, _)) = sapphire_framework::ipc::connect_or_absent(&endpoint, app, info)
        .await
        .context("connecting to the running agent")?
    else {
        bail!(
            "{app} is not running on this host; `{app} acp` relays to a running agent and \
             starts none. Start it with `{app} serve`, or install it as a service with \
             `{app} service install`"
        );
    };
    relay(
        Arc::new(client),
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// The id [`OPENED`] carries.
async fn wait_opened(
    events: &mut tokio::sync::broadcast::Receiver<sapphire_framework::ipc::Notification>,
) -> Result<String> {
    loop {
        let event = events
            .recv()
            .await
            .context("the agent went away before opening a connection")?;
        if event.method == OPENED {
            return event.params["conn"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("{OPENED} carried no connection id"));
        }
    }
}

/// Forward [`RECV`] lines for `conn` until [`CLOSED`].
async fn collect_lines(
    mut events: tokio::sync::broadcast::Receiver<sapphire_framework::ipc::Notification>,
    conn: String,
    out: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(n)) => {
                // A lost line is a corrupt conversation: stop rather than
                // carry on without it.
                bail!("{n} lines from the agent were lost; closing the connection");
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        if event.params["conn"].as_str() != Some(conn.as_str()) {
            continue;
        }
        match event.method.as_str() {
            RECV => {
                if let Some(line) = event.params["line"].as_str()
                    && out.send(line.to_owned()).is_err()
                {
                    return Ok(());
                }
            }
            CLOSED => return Ok(()),
            _ => {}
        }
    }
}

/// Write each line out with its newline: ACP's stdio framing.
async fn write_lines<W: AsyncWrite + Unpin>(
    mut lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut output: W,
) -> Result<()> {
    while let Some(line) = lines.recv().await {
        output.write_all(line.as_bytes()).await?;
        output.write_all(b"\n").await?;
        output.flush().await?;
    }
    Ok(())
}

/// Send the editor's lines one at a time until it closes its end.
async fn pump_input<R: AsyncBufRead + Unpin>(client: &Client, conn: &str, input: R) -> Result<()> {
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        client
            .call::<_, Value>(SEND, json!({ "conn": conn, "line": line }))
            .await
            .context("relaying a line to the agent")?;
    }
    Ok(())
}

/// Wait for the agent's last lines to be written, then report how the
/// connection ended.
async fn finish(
    answered: std::result::Result<Value, sapphire_framework::ipc::Error>,
    collect: tokio::task::JoinHandle<Result<()>>,
    write: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let ended = match answered {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow!(e).context("the agent's connection was lost")),
    };
    // After a clean end [`CLOSED`] has arrived or is on its way; after a
    // lost server nothing will come, so the collector is stopped instead.
    if ended.is_err() {
        collect.abort();
    }
    let collected = match collect.await {
        Ok(r) => r,
        Err(e) if e.is_cancelled() => Ok(()),
        Err(e) => Err(anyhow!(e)),
    };
    let written = write.await.context("the writer panicked")?;
    ended.and(collected).and(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_framework::ipc::{Connection, ManagedBy, ServerInfo, serve};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    const APP: &str = "sapphire-agent";

    fn server_info() -> ServerInfo {
        ServerInfo {
            version: "test".into(),
            api: 1,
            app_api: Some(APP_API),
            pid: std::process::id(),
            managed_by: ManagedBy::Service,
        }
    }

    fn client_info() -> ClientInfo {
        ClientInfo {
            kind: "acp".into(),
            version: "test".into(),
            api: 1,
            app_api: Some(APP_API),
            pid: std::process::id(),
        }
    }

    /// A tunnel served on one end of an in-process IPC connection, and a
    /// client handshaken on the other.
    async fn connected(tunnel: &AcpTunnel) -> Arc<Client> {
        let (server_end, client_end) = Connection::pair();
        let router = Arc::new(tunnel.router(Router::new()));
        tokio::spawn(async move { serve(server_end, router, APP, server_info()).await });
        let (client, _) = Client::handshake(client_end, APP, client_info())
            .await
            .unwrap();
        Arc::new(client)
    }

    /// The editor's side of a relay: its stdin to write into, its stdout to
    /// read from, and the relay itself.
    struct Editor {
        stdin: Option<DuplexStream>,
        stdout: tokio::io::Lines<BufReader<DuplexStream>>,
        relay: tokio::task::JoinHandle<Result<()>>,
    }

    impl Editor {
        fn start(client: Arc<Client>) -> Editor {
            let (stdin, relay_in) = tokio::io::duplex(1 << 16);
            let (relay_out, stdout) = tokio::io::duplex(1 << 16);
            let relay = tokio::spawn(relay(client, BufReader::new(relay_in), relay_out));
            Editor {
                stdin: Some(stdin),
                stdout: BufReader::new(stdout).lines(),
                relay,
            }
        }

        async fn send(&mut self, v: Value) {
            let stdin = self.stdin.as_mut().unwrap();
            stdin.write_all(v.to_string().as_bytes()).await.unwrap();
            stdin.write_all(b"\n").await.unwrap();
        }

        async fn next(&mut self) -> Value {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(10), self.stdout.next_line())
                    .await
                    .expect("the agent answers in time")
                    .unwrap()
                    .expect("the relay is still writing");
            serde_json::from_str(&line).unwrap()
        }

        /// Read until the response to request `id`, answering whatever the
        /// agent asks the editor on the way with `answer`.
        async fn until_response(
            &mut self,
            id: i64,
            mut answer: impl FnMut(&Value) -> Option<Value>,
        ) -> (Value, Vec<Value>) {
            let mut seen = Vec::new();
            loop {
                let v = self.next().await;
                if v["id"] == id && v.get("method").is_none() {
                    return (v, seen);
                }
                if let Some(reply) = answer(&v) {
                    self.send(reply).await;
                }
                seen.push(v);
            }
        }
    }

    fn request(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn initialize(id: i64) -> Value {
        request(
            id,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
                "clientInfo": { "name": "test-editor", "version": "0.0.0" }
            }),
        )
    }

    fn new_session(id: i64) -> Value {
        request(
            id,
            "session/new",
            json!({ "cwd": "/work/proj", "mcpServers": [] }),
        )
    }

    fn prompt(id: i64, session: &str, text: &str) -> Value {
        request(
            id,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        )
    }

    fn reply_text(text: &str) -> crate::provider::ChatResponse {
        crate::provider::ChatResponse {
            prompt_usage: None,
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: None,
        }
    }

    /// `initialize` → `session/new` → `session/prompt`, through the relay
    /// and the IPC tunnel, ends in the agent's reply.
    #[tokio::test]
    async fn a_prompt_round_trips_through_the_tunnel() {
        let tunnel = AcpTunnel::default();
        tunnel.attach(ServeState::for_test_scripted(
            false,
            vec![reply_text("hello back")],
        ));
        let mut editor = Editor::start(connected(&tunnel).await);

        editor.send(initialize(0)).await;
        let (init, _) = editor.until_response(0, |_| None).await;
        assert_eq!(init["result"]["protocolVersion"], 1, "got {init}");

        editor.send(new_session(1)).await;
        let (created, _) = editor.until_response(1, |_| None).await;
        let session = created["result"]["sessionId"].as_str().unwrap().to_owned();

        editor.send(prompt(2, &session, "hello")).await;
        let (done, seen) = editor.until_response(2, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "got {done}");
        assert!(
            seen.iter()
                .any(|v| v["params"]["update"]["content"]["text"] == "hello back"),
            "the reply streams back as an update: {seen:?}"
        );
    }

    /// The agent asking the editor — here, for permission — travels the
    /// tunnel the other way, and the editor's answer travels back.
    #[tokio::test]
    async fn the_agent_asks_the_editor_through_the_tunnel() {
        let state = ServeState::for_test_scripted(
            false,
            vec![
                crate::provider::ChatResponse {
                    prompt_usage: None,
                    text: None,
                    tool_calls: vec![crate::provider::ToolCall {
                        id: "call-1".to_string(),
                        name: "risky".to_string(),
                        input: json!({}),
                    }],
                    stop_reason: None,
                },
                reply_text("done"),
            ],
        );
        let risky = crate::serve::RiskyTool::new();
        let ran = risky.ran_flag();
        state.tools.register_tool(Box::new(risky)).await;
        let tunnel = AcpTunnel::default();
        tunnel.attach(state);
        let mut editor = Editor::start(connected(&tunnel).await);

        editor.send(initialize(0)).await;
        editor.until_response(0, |_| None).await;
        editor.send(new_session(1)).await;
        let (created, _) = editor.until_response(1, |_| None).await;
        let session = created["result"]["sessionId"].as_str().unwrap().to_owned();

        editor.send(prompt(2, &session, "run it")).await;
        let mut asked = 0;
        let (done, _) = editor
            .until_response(2, |v| {
                (v["method"] == "session/request_permission").then(|| {
                    asked += 1;
                    json!({
                        "jsonrpc": "2.0",
                        "id": v["id"],
                        "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
                    })
                })
            })
            .await;
        assert_eq!(asked, 1, "the agent asked once");
        assert_eq!(done["result"]["stopReason"], "end_turn", "got {done}");
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "allowed, so it ran"
        );
    }

    /// Closing the editor's stdin ends the relay cleanly and releases the
    /// connection on the server.
    #[tokio::test]
    async fn closing_stdin_closes_the_connection() {
        let tunnel = AcpTunnel::default();
        tunnel.attach(ServeState::for_test(false));
        let mut editor = Editor::start(connected(&tunnel).await);
        editor.send(initialize(0)).await;
        editor.until_response(0, |_| None).await;

        editor.stdin = None;
        tokio::time::timeout(std::time::Duration::from_secs(10), &mut editor.relay)
            .await
            .expect("the relay ends")
            .unwrap()
            .expect("cleanly");
        assert!(tunnel.inner.conns.lock().unwrap().is_empty());
    }

    /// The IPC connection going away — the relay killed — closes the ACP
    /// connection on the server, and runs its teardown: the sessions it
    /// held are released.
    #[tokio::test]
    async fn a_lost_ipc_connection_closes_the_acp_connection() {
        let state = ServeState::for_test(false);
        let tunnel = AcpTunnel::default();
        tunnel.attach(Arc::clone(&state));
        let client = connected(&tunnel).await;
        let mut editor = Editor::start(Arc::clone(&client));
        editor.send(initialize(0)).await;
        editor.until_response(0, |_| None).await;
        editor.send(new_session(1)).await;
        editor.until_response(1, |_| None).await;
        assert_eq!(state.open_acp_sessions.lock().await.len(), 1);

        editor.relay.abort();
        let _ = (&mut editor.relay).await;
        drop(client);

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !tunnel.inner.conns.lock().unwrap().is_empty()
                || !state.open_acp_sessions.lock().await.is_empty()
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the connection is released");
    }

    #[tokio::test]
    async fn without_a_stdio_room_profile_the_relay_says_what_to_set() {
        let mut state = ServeState::for_test(false);
        Arc::get_mut(&mut state)
            .unwrap()
            .config
            .acp
            .as_mut()
            .unwrap()
            .stdio_room_profile = None;
        let tunnel = AcpTunnel::default();
        tunnel.attach(state);
        let editor = Editor::start(connected(&tunnel).await);

        let err = editor.relay.await.unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("[acp].stdio_room_profile"),
            "got {err:#}"
        );
    }

    #[tokio::test]
    async fn before_the_agent_is_built_the_relay_is_told_to_wait() {
        let tunnel = AcpTunnel::default();
        let editor = Editor::start(connected(&tunnel).await);
        let err = editor.relay.await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("still starting"), "got {err:#}");
    }

    /// One relay cannot write into another's conversation.
    #[tokio::test]
    async fn a_connection_takes_lines_only_from_its_owner() {
        let tunnel = AcpTunnel::default();
        tunnel.inner.conns.lock().unwrap().insert(
            "c".into(),
            Conn {
                owner: 1,
                lines: tokio::sync::mpsc::channel(1).0,
                cancel: CancellationToken::new(),
            },
        );
        let client = connected(&tunnel).await;
        let err = client
            .call::<_, Value>(SEND, json!({ "conn": "c", "line": "{}" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no ACP connection"), "got {err}");
    }
}
