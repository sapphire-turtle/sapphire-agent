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

    /// Sync from `source` unless a sync is already running. Returns whether
    /// one was started; on `false` the caller keeps its request and retries.
    pub fn submit(&self, source: Arc<dyn AvatarSource>) -> bool {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return false;
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
        true
    }

    fn try_recv(&self) -> Option<anyhow::Result<LoadedAvatar>> {
        self.rx.lock().ok()?.try_recv().ok()
    }
}

/// Whether a sync is wanted: one was requested, or [`POLL_INTERVAL`] of
/// real time passed since the last one started. Measured on the wall clock
/// (`Time<Real>`), not frames, because the GUI idles between wakes.
fn wanted(now: Duration, last_started: Duration, requested: bool) -> bool {
    requested || now.saturating_sub(last_started) >= POLL_INTERVAL
}

/// Starts a sync when one is [`wanted`]; needs the live connection, so it
/// waits for one. A request stays pending until a sync actually starts, so
/// one made during a running sync is retried on later frames.
pub(crate) fn poll_sync(
    time: Res<Time<Real>>,
    mut last_started: Local<Duration>,
    mut requested: ResMut<SyncRequested>,
    bridge: Res<ChatBridge>,
    sync: Res<AvatarSync>,
) {
    let now = time.elapsed();
    if !wanted(now, *last_started, requested.0) {
        return;
    }
    if let Some(client) = bridge.client()
        && sync.submit(client)
    {
        requested.0 = false;
        *last_started = now;
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Idle;

    #[async_trait::async_trait]
    impl AvatarSource for Idle {
        async fn manifest(&self) -> anyhow::Result<sapphire_agent_core::AvatarManifest> {
            Ok(Default::default())
        }
        async fn fetch(&self, _: &str) -> anyhow::Result<Vec<u8>> {
            unreachable!()
        }
    }

    #[test]
    fn the_poll_is_due_after_300_seconds_of_real_time() {
        let s = Duration::from_secs;
        assert!(!wanted(s(299), s(0), false));
        assert!(wanted(s(300), s(0), false));
        // Measured from the last start, however few frames ran between.
        assert!(!wanted(s(500), s(300), false));
        assert!(wanted(s(600), s(300), false));
        assert!(wanted(s(1), s(0), true));
    }

    #[test]
    fn a_request_made_during_a_sync_survives_and_runs_after_it() {
        // Not driven: the spawned sync stays in flight until the runtime runs.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let sync = AvatarSync::new(runtime.handle().clone(), Arc::new(|| {}));
        let source: Arc<dyn AvatarSource> = Arc::new(Idle);

        assert!(sync.submit(Arc::clone(&source)));
        // A new request arrives while that sync runs: it is refused, so
        // `poll_sync` leaves `SyncRequested` set and asks again.
        assert!(!sync.submit(Arc::clone(&source)));

        runtime.block_on(async {
            while sync.in_flight.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        });
        assert!(sync.submit(source), "runs once the first sync is done");
    }
}
