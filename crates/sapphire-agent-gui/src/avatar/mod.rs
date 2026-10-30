//! The companion's avatar: its files, synced from the agent into a local
//! cache ([`assets`]), and the bevy side that keeps them current.

pub mod assets;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::Waker;
use crate::chat::{ChatBridge, SyncRequested};
pub use assets::{AvatarAssets, AvatarSource, Cache, LoadedAvatar};

/// How often the agent is asked whether the avatar changed.
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Runs syncs on the GUI's runtime and hands their results to
/// [`drain_sync`]. One sync runs at a time: a request made while one is in
/// flight is dropped, since that sync reads the agent's newest manifest
/// anyway or the next poll will.
#[derive(Resource)]
pub struct AvatarSync {
    runtime: Handle,
    waker: Waker,
    cache_dir: Option<std::path::PathBuf>,
    in_flight: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<anyhow::Result<LoadedAvatar>>,
    rx: Mutex<mpsc::UnboundedReceiver<anyhow::Result<LoadedAvatar>>>,
}

impl AvatarSync {
    pub fn new(runtime: Handle, waker: Waker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            runtime,
            waker,
            cache_dir: Cache::default_dir(),
            in_flight: Arc::new(AtomicBool::new(false)),
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// Sync from `source` unless a sync is already running.
    pub fn submit(&self, source: Arc<dyn AvatarSource>) {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let cache_dir = self.cache_dir.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let tx = self.tx.clone();
        let waker = Arc::clone(&self.waker);
        self.runtime.spawn(async move {
            let result = match cache_dir {
                Some(dir) => assets::sync(source.as_ref(), &Cache::new(dir)).await,
                None => Err(anyhow::anyhow!("this platform has no cache directory")),
            };
            in_flight.store(false, Ordering::Release);
            let _ = tx.send(result);
            waker();
        });
    }

    fn try_recv(&self) -> Option<anyhow::Result<LoadedAvatar>> {
        self.rx.lock().ok()?.try_recv().ok()
    }
}

/// Counts down to the next poll of the agent's manifest.
pub(crate) struct PollTimer(Timer);

impl Default for PollTimer {
    fn default() -> Self {
        Self(Timer::new(POLL_INTERVAL, TimerMode::Repeating))
    }
}

/// Starts a sync when the poll interval elapsed or a connection was just
/// made ([`SyncRequested`]); needs the live connection, so it waits for one.
pub(crate) fn poll_sync(
    time: Res<Time>,
    mut timer: Local<PollTimer>,
    mut requested: ResMut<SyncRequested>,
    bridge: Res<ChatBridge>,
    sync: Res<AvatarSync>,
) {
    let due = timer.0.tick(time.delta()).just_finished();
    if !(due || requested.0) {
        return;
    }
    if let Some(client) = bridge.client() {
        requested.0 = false;
        sync.submit(client);
    }
}

/// Folds finished syncs into [`AvatarAssets`].
pub(crate) fn drain_sync(sync: Res<AvatarSync>, mut assets: ResMut<AvatarAssets>) {
    while let Some(result) = sync.try_recv() {
        if let Err(e) = &result {
            tracing::warn!("avatar sync failed: {e:#}");
        }
        assets.apply(result);
    }
}
