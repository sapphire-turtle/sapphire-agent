//! The chat with the agent running on this host: its state
//! ([`ChatState`]), the async bridge to the agent ([`RpcBridge`]) and the
//! egui screen (`view`).

mod bridge;
mod state;
pub(crate) mod view;

use std::sync::Mutex;
use std::time::Duration;

use bevy::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::avatar::AvatarState;
use crate::{GuiRuntime, GuiWaker};

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

/// How long after a turn's reply the avatar goes back to neutral and its
/// default pose.
const REVERT_AFTER: Duration = Duration::from_secs(30);

/// Drains every event the tokio side has produced this frame and
/// folds it into `ChatState`; the avatar's cues and the end of a turn go
/// to `AvatarState`. Idempotent if the queue is empty.
pub(crate) fn drain_bridge_events(
    mut state: ResMut<ChatState>,
    mut avatar: ResMut<AvatarState>,
    queue: Res<BridgeEventQueue>,
    mut sync: ResMut<SyncRequested>,
    time: Res<Time<Real>>,
    runtime: Res<GuiRuntime>,
    waker: Res<GuiWaker>,
) {
    let Ok(mut rx) = queue.0.lock() else {
        return;
    };
    while let Ok(evt) = rx.try_recv() {
        match &evt {
            BridgeEvent::SessionReady { .. } => sync.0 = true,
            BridgeEvent::Expression { name, intensity } => avatar.expression(name, *intensity),
            BridgeEvent::Action { name } => avatar.action(name),
            BridgeEvent::Pose { name } => avatar.pose(name),
            BridgeEvent::ChatText { .. } | BridgeEvent::ChatError { .. } => {
                avatar.turn_finished(time.elapsed(), REVERT_AFTER);
                // An idle window draws no frame on its own, so wake it when
                // the revert is due.
                let waker = waker.0.clone();
                runtime.0.spawn(async move {
                    tokio::time::sleep(REVERT_AFTER).await;
                    waker();
                });
            }
            _ => {}
        }
        state.apply(evt);
    }
}
