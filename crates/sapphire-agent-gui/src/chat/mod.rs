//! The chat with the agent running on this host: its state
//! ([`ChatState`]), the async bridge to the agent ([`RpcBridge`]) and the
//! egui screen (`view`).

mod bridge;
mod state;
pub(crate) mod view;

use std::sync::Mutex;

use bevy::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;

pub use bridge::{BridgeEvent, RpcBridge};
pub use state::{ChatEntry, ChatRole, ChatState, Session, TurnState};

/// The bridge as a bevy `Resource`. `bridge` itself imports nothing from
/// bevy, so it stays usable outside it.
#[derive(Resource, Deref)]
pub(crate) struct ChatBridge(pub(crate) RpcBridge);

/// Wraps the bridge receiver in a `Mutex` so it can be a bevy
/// `Resource` (which requires `Send + Sync`). Only one system reads it.
#[derive(Resource)]
pub(crate) struct BridgeEventQueue(Mutex<UnboundedReceiver<BridgeEvent>>);

impl BridgeEventQueue {
    pub(crate) fn new(events: UnboundedReceiver<BridgeEvent>) -> Self {
        Self(Mutex::new(events))
    }
}

/// Set when a session opens: the avatar should be synced now rather than
/// at the next poll. Whoever syncs clears it.
#[derive(Resource, Default)]
pub struct SyncRequested(pub bool);

/// Drains every event the tokio side has produced this frame and
/// folds it into `ChatState`. Idempotent if the queue is empty.
pub(crate) fn drain_bridge_events(
    mut state: ResMut<ChatState>,
    queue: Res<BridgeEventQueue>,
    mut sync: ResMut<SyncRequested>,
) {
    let Ok(mut rx) = queue.0.lock() else {
        return;
    };
    while let Ok(evt) = rx.try_recv() {
        if matches!(evt, BridgeEvent::SessionReady { .. }) {
            sync.0 = true;
        }
        state.apply(evt);
    }
}
