//! Async bridge between tokio and the bevy main thread.
//!
//! Bevy systems run sync, but the agent client (`sapphire-agent-core`) is
//! all `async`. We park a multi-threaded tokio runtime in a bevy
//! `Resource`, expose `submit_*` helpers that spawn async tasks on it,
//! and hand a `tokio::sync::mpsc::UnboundedReceiver<BridgeEvent>` to
//! the UI side which drains it once per frame via `try_recv`.
//!
//! The agent is the one running on this host, reached over its local IPC
//! endpoint like `sapphire-agent chat` — no URL, no token.
//!
//! Pure-ish module: imports nothing from bevy. The bevy `Resource`
//! wrapper lives in `main.rs` so this stays portable to other GUI
//! frameworks if we ever swap bevy out.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use sapphire_agent_core::{ChatClient, ChatEvent, Turn};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// Events flowing tokio → bevy. The UI converts these into `ChatEntry`
/// updates and `Session` state transitions.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// Connected and a session is open; chat can begin.
    SessionReady {
        display_id: String,
    },
    /// Connecting or opening failed — the user can retry.
    SessionFailed {
        message: String,
    },
    /// A chat turn's reply. Always the last event for a successful turn.
    ChatText {
        text: String,
    },
    /// A tool call in the turn started or ended.
    ToolStart {
        name: String,
    },
    ToolEnd {
        name: String,
    },
    /// The turn failed.
    ChatError {
        message: String,
    },
}

/// The connection and the session it opened, once there are both.
struct Live {
    client: Arc<ChatClient>,
    session_id: String,
}

/// Owns the tokio runtime, the live connection and the outgoing event
/// channel.
pub struct RpcBridge {
    runtime: Arc<Runtime>,
    live: Arc<Mutex<Option<Live>>>,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
}

impl RpcBridge {
    pub fn new() -> Result<(Self, mpsc::UnboundedReceiver<BridgeEvent>)> {
        // Multi-threaded so an in-flight turn doesn't hold up a reconnect
        // on the same thread.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("sapphire-agent-desktop-rt")
            .build()?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Ok((
            Self {
                runtime: Arc::new(runtime),
                live: Arc::new(Mutex::new(None)),
                event_tx,
            },
            event_rx,
        ))
    }

    /// Connect to the agent running on this host and open a new session.
    /// Emits exactly one of `SessionReady` / `SessionFailed`.
    pub fn submit_connect(&self) {
        let live = Arc::clone(&self.live);
        let tx = self.event_tx.clone();
        self.runtime.spawn(async move {
            let opened = async {
                let client = ChatClient::connect("desktop", env!("CARGO_PKG_VERSION")).await?;
                let opened = client.open(None).await?;
                anyhow::Ok((client, opened))
            }
            .await;
            let evt = match opened {
                Ok((client, opened)) => {
                    *live.lock().unwrap() = Some(Live {
                        client: Arc::new(client),
                        session_id: opened.session_id,
                    });
                    BridgeEvent::SessionReady {
                        display_id: opened.display_id,
                    }
                }
                Err(e) => {
                    *live.lock().unwrap() = None;
                    BridgeEvent::SessionFailed {
                        message: format!("{e:#}"),
                    }
                }
            };
            let _ = tx.send(evt);
        });
    }

    /// Send a chat turn in the open session.
    pub fn submit_chat(&self, content: String) {
        let live = self
            .live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| (Arc::clone(&l.client), l.session_id.clone()));
        let tx = self.event_tx.clone();
        let Some((client, session_id)) = live else {
            let _ = tx.send(BridgeEvent::ChatError {
                message: "not connected".to_owned(),
            });
            return;
        };
        self.runtime.spawn(async move {
            let tool_tx = tx.clone();
            let reply = client
                .send(&session_id, Turn::text(&content), |event| {
                    if let ChatEvent::Tool(event) = event {
                        let _ = tool_tx.send(if event.started {
                            BridgeEvent::ToolStart { name: event.name }
                        } else {
                            BridgeEvent::ToolEnd { name: event.name }
                        });
                    }
                })
                .await;
            let _ = tx.send(match reply {
                Ok(text) => BridgeEvent::ChatText { text },
                Err(e) => BridgeEvent::ChatError {
                    message: format!("{e:#}"),
                },
            });
        });
    }
}
