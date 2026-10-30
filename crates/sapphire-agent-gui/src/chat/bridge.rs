//! Async bridge between tokio and the bevy main thread.
//!
//! Bevy systems run sync, but the agent client (`sapphire-agent-core`) is
//! all `async`. The bridge spawns its work on the GUI's tokio runtime
//! (`GuiRuntime`, through a `Handle`), and hands a
//! `tokio::sync::mpsc::UnboundedReceiver<BridgeEvent>` to the UI side,
//! which drains it once per frame via `try_recv`. Every event is followed
//! by one call of the [`Waker`], so an idle window draws the frame that
//! shows it.
//!
//! The agent is the one running on this host, reached over its local IPC
//! endpoint like `sapphire-agent chat` — no URL, no token.
//!
//! Pure-ish module: imports nothing from bevy. The bevy `Resource`
//! wrapper lives in `chat/mod.rs` so this stays portable to other GUI
//! frameworks if we ever swap bevy out.

use std::sync::{Arc, Mutex};

use sapphire_agent_core::{ChatClient, ChatEvent, ClientCaps, Turn};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::Waker;

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
    /// The agent set the avatar's expression.
    Expression {
        name: String,
        intensity: f32,
    },
    /// The agent asked the avatar to play an action once.
    Action {
        name: String,
    },
    /// The agent set the pose the avatar holds.
    Pose {
        name: String,
    },
}

impl BridgeEvent {
    /// The bridge event for a turn's progress event, if it has one.
    fn from_progress(event: ChatEvent) -> Option<Self> {
        Some(match event {
            ChatEvent::Tool(tool) if tool.started => Self::ToolStart { name: tool.name },
            ChatEvent::Tool(tool) => Self::ToolEnd { name: tool.name },
            ChatEvent::Expression { name, intensity } => Self::Expression { name, intensity },
            ChatEvent::Action { name } => Self::Action { name },
            ChatEvent::Pose { name } => Self::Pose { name },
            _ => return None,
        })
    }
}

/// The connection and the session it opened, once there are both.
struct Live {
    client: Arc<ChatClient>,
    session_id: String,
}

/// The outgoing event channel and the waker that follows each send.
#[derive(Clone)]
struct Events {
    tx: mpsc::UnboundedSender<BridgeEvent>,
    waker: Waker,
}

impl Events {
    fn send(&self, event: BridgeEvent) {
        let _ = self.tx.send(event);
        (self.waker)();
    }
}

/// Holds the runtime handle, the live connection and the outgoing event
/// channel.
pub struct RpcBridge {
    runtime: Handle,
    live: Arc<Mutex<Option<Live>>>,
    events: Events,
    client_version: &'static str,
}

impl RpcBridge {
    pub fn new(runtime: Handle, waker: Waker) -> (Self, mpsc::UnboundedReceiver<BridgeEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                runtime,
                live: Arc::new(Mutex::new(None)),
                events: Events { tx, waker },
                client_version: env!("CARGO_PKG_VERSION"),
            },
            rx,
        )
    }

    /// The version the bridge reports to the agent when it connects: the
    /// app's, not this library's. Defaults to this library's.
    pub fn client_version(mut self, version: &'static str) -> Self {
        self.client_version = version;
        self
    }

    /// Connect to the agent running on this host and open a new session.
    /// Emits exactly one of `SessionReady` / `SessionFailed`.
    pub fn submit_connect(&self) {
        let live = Arc::clone(&self.live);
        let events = self.events.clone();
        let version = self.client_version;
        self.runtime.spawn(async move {
            let opened = async {
                let client = ChatClient::connect("desktop", version).await?;
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
            events.send(evt);
        });
    }

    /// The live connection, once there is one. Other work on the same
    /// agent (the avatar sync) shares it.
    pub fn client(&self) -> Option<Arc<ChatClient>> {
        self.live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| Arc::clone(&l.client))
    }

    /// [`Self::client`] as a function that can outlive the bridge's borrow,
    /// for work on another thread.
    pub fn client_lookup(&self) -> crate::avatar::assets::ClientLookup {
        let live = Arc::clone(&self.live);
        Arc::new(move || live.lock().unwrap().as_ref().map(|l| Arc::clone(&l.client)))
    }

    /// Send a chat turn in the open session, declaring what this client
    /// can show for it (`caps`).
    pub fn submit_chat(&self, content: String, caps: ClientCaps) {
        let live = self
            .live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| (Arc::clone(&l.client), l.session_id.clone()));
        let events = self.events.clone();
        let Some((client, session_id)) = live else {
            events.send(BridgeEvent::ChatError {
                message: "not connected".to_owned(),
            });
            return;
        };
        self.runtime.spawn(async move {
            let progress = events.clone();
            let turn = Turn {
                content: &content,
                attachments: &[],
                client: &caps,
            };
            let reply = client
                .send(&session_id, turn, |event| {
                    if let Some(event) = BridgeEvent::from_progress(event) {
                        progress.send(event);
                    }
                })
                .await;
            events.send(match reply {
                Ok(text) => BridgeEvent::ChatText { text },
                Err(e) => BridgeEvent::ChatError {
                    message: format!("{e:#}"),
                },
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sapphire_agent_core::ClientCaps;

    use super::*;

    #[test]
    fn progress_maps_cues_and_keeps_tools_apart() {
        use sapphire_agent_core::ToolEvent;

        assert!(matches!(
            BridgeEvent::from_progress(ChatEvent::Expression {
                name: "happy".into(),
                intensity: 0.5,
            }),
            Some(BridgeEvent::Expression { name, intensity }) if name == "happy" && intensity == 0.5
        ));
        assert!(matches!(
            BridgeEvent::from_progress(ChatEvent::Action { name: "wave".into() }),
            Some(BridgeEvent::Action { name }) if name == "wave"
        ));
        assert!(matches!(
            BridgeEvent::from_progress(ChatEvent::Pose { name: "arms".into() }),
            Some(BridgeEvent::Pose { name }) if name == "arms"
        ));
        let tool = |started| {
            ChatEvent::Tool(ToolEvent {
                started,
                id: "1".into(),
                name: "set_pose".into(),
            })
        };
        assert!(matches!(
            BridgeEvent::from_progress(tool(true)),
            Some(BridgeEvent::ToolStart { name }) if name == "set_pose"
        ));
        assert!(matches!(
            BridgeEvent::from_progress(tool(false)),
            Some(BridgeEvent::ToolEnd { name }) if name == "set_pose"
        ));
    }

    #[test]
    fn submit_chat_without_connection_reports_and_wakes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let waker: crate::Waker = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let (bridge, mut rx) = RpcBridge::new(runtime.handle().clone(), waker);
        bridge.submit_chat("x".to_owned(), ClientCaps::default());
        match rx.try_recv() {
            Ok(BridgeEvent::ChatError { message }) => assert_eq!(message, "not connected"),
            other => panic!("expected ChatError, got {other:?}"),
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }
}
