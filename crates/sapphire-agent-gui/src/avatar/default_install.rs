//! The first-run flow for the default avatar. [`Flow`] is a pure state
//! machine: no bevy, no IO. The caller feeds it what happened (the manifest
//! was checked, progress arrived, the agent answered, the user pressed a
//! button) and sends the [`Request`] it returns. [`DefaultAvatar`] is that
//! caller: it runs the checks and the requests on the GUI's runtime, and
//! [`drive_default_avatar`] folds what they report back into the flow.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use sapphire_agent_core::{ChatClient, DefaultAvatarAnswer, InstallProgress};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::AvatarAssets;
use crate::Waker;
use crate::chat::{ChatBridge, SyncRequested};

/// Where the first-run flow stands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Flow {
    /// Connected, the manifest not yet checked.
    Checking,
    /// No avatar: the agent is asked, and has not said yet whether it will
    /// download one. The normal screen stays, so an answer that downloads
    /// nothing never flashes the loading screen.
    Asking,
    /// The agent is downloading the default avatar.
    Loading { received: u64, total: Option<u64> },
    /// Installed; the avatar is syncing to the GUI before the normal screen.
    Reloading,
    /// The download failed; the screen stays blocked until retry or continue.
    Failed { message: String },
    /// The normal screen. `offer`: the Character page shows "Get the default avatar".
    Done { offer: bool },
}

/// A request to send: `install_default { auto }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub auto: bool,
}

impl Flow {
    /// A new connection: nothing checked yet.
    pub(crate) fn new() -> Self {
        Self::Checking
    }

    fn start_loading(&mut self, auto: bool) -> Option<Request> {
        *self = Flow::Loading {
            received: 0,
            total: None,
        };
        Some(Request { auto })
    }

    /// The manifest was checked. Only meaningful while `Checking`; any other
    /// state ignores it, so a continued-without flow never retries by itself.
    pub(crate) fn checked(&mut self, has_avatar: bool) -> Option<Request> {
        if *self != Flow::Checking {
            return None;
        }
        if has_avatar {
            *self = Flow::Done { offer: false };
            None
        } else {
            *self = Flow::Asking;
            Some(Request { auto: true })
        }
    }

    /// Download progress. The first one while `Asking` means the agent is
    /// downloading, and blocks the screen. Applies only while `Asking` or
    /// `Loading`, so a stale one after the answer cannot reopen the
    /// loading screen.
    pub(crate) fn progress(&mut self, p: InstallProgress) {
        if matches!(self, Flow::Asking | Flow::Loading { .. }) {
            *self = Flow::Loading {
                received: p.received,
                total: p.total,
            };
        }
    }

    /// The agent answered (or the call failed). Returns whether to resync the
    /// avatar now. Applies only while `Asking` or `Loading`.
    pub(crate) fn answered(&mut self, a: Result<DefaultAvatarAnswer, String>) -> bool {
        let asking = match self {
            Flow::Asking => true,
            Flow::Loading { .. } => false,
            _ => return false,
        };
        match a {
            // Nothing was downloaded, so nothing new will load: no reason
            // to block. The resync is only for a race with another client.
            Ok(DefaultAvatarAnswer::AlreadyPresent) if asking => {
                *self = Flow::Done { offer: false };
                true
            }
            Ok(DefaultAvatarAnswer::Installed | DefaultAvatarAnswer::AlreadyPresent) => {
                *self = Flow::Reloading;
                true
            }
            Ok(DefaultAvatarAnswer::NeedsConfirmation) => {
                *self = Flow::Done { offer: true };
                false
            }
            Ok(DefaultAvatarAnswer::Unavailable) => {
                *self = Flow::Done { offer: false };
                false
            }
            Err(message) => {
                *self = Flow::Failed { message };
                false
            }
        }
    }

    /// The resync after the install finished (see [`resync_finished`]).
    /// Applies only while `Reloading`.
    pub(crate) fn reloaded(&mut self) {
        if *self == Flow::Reloading {
            *self = Flow::Done { offer: false };
        }
    }

    /// Try again after a failure (never automatic).
    pub(crate) fn retry(&mut self) -> Option<Request> {
        if matches!(self, Flow::Failed { .. }) {
            self.start_loading(false)
        } else {
            None
        }
    }

    /// Give up after a failure; the Character page offers the button instead.
    pub(crate) fn continue_without(&mut self) {
        if matches!(self, Flow::Failed { .. }) {
            *self = Flow::Done { offer: true };
        }
    }

    /// The user asked for the default avatar from the Character page.
    pub(crate) fn get(&mut self) -> Option<Request> {
        if *self == (Flow::Done { offer: true }) {
            self.start_loading(false)
        } else {
            None
        }
    }

    /// Whether the loading or failure screen replaces the normal one.
    pub(crate) fn blocks_screen(&self) -> bool {
        matches!(
            self,
            Flow::Loading { .. } | Flow::Reloading | Flow::Failed { .. }
        )
    }
}

/// Whether the avatar resync asked for at the answer has finished: the
/// loaded avatar changed since `captured` (its generation then), or a sync
/// failed, which ends the wait too rather than block the GUI.
fn resync_finished(captured: u64, current: u64, error: bool) -> bool {
    current != captured || error
}

/// Whether `live` is a new connection rather than the one `stored` belongs
/// to: the first one, or a reconnect. Losing the connection (`None`) is no
/// change; the flow keeps its state until the next one comes.
fn connection_changed<T>(stored: Option<&Arc<T>>, live: Option<&Arc<T>>) -> bool {
    match (stored, live) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(stored), Some(live)) => !Arc::ptr_eq(stored, live),
    }
}

/// What a job on the runtime reports.
enum Event {
    Checked { has_avatar: bool },
    Progress(InstallProgress),
    Answered(Result<DefaultAvatarAnswer, String>),
}

/// The default avatar's flow for the live connection, and the jobs that
/// drive it on the GUI's runtime. Each event carries the generation of the
/// connection it was made for, and one from an older connection is
/// dropped, so a reconnect's flow never sees the previous one's answer.
#[derive(Resource)]
pub(crate) struct DefaultAvatar {
    flow: Flow,
    /// The connection the flow belongs to.
    client: Option<Arc<ChatClient>>,
    /// Bumped with each new connection.
    generation: u64,
    /// [`AvatarAssets::generation`] when the install was answered, for
    /// [`Flow::Reloading`] to wait past.
    reload_from: u64,
    runtime: Handle,
    waker: Waker,
    tx: mpsc::UnboundedSender<(u64, Event)>,
    rx: Mutex<mpsc::UnboundedReceiver<(u64, Event)>>,
}

impl DefaultAvatar {
    pub(crate) fn new(runtime: Handle, waker: Waker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            flow: Flow::new(),
            client: None,
            generation: 0,
            reload_from: 0,
            runtime,
            waker,
            tx,
            rx: Mutex::new(rx),
        }
    }

    pub(crate) fn flow(&self) -> &Flow {
        &self.flow
    }

    /// Try the download again after a failure.
    pub(crate) fn retry(&mut self) {
        if let Some(request) = self.flow.retry() {
            self.send(request);
        }
    }

    /// Give up after a failure; the Character page offers the button.
    pub(crate) fn continue_without(&mut self) {
        self.flow.continue_without();
    }

    /// The Character page's "Get the default avatar".
    pub(crate) fn get(&mut self) {
        if let Some(request) = self.flow.get() {
            self.send(request);
        }
    }

    /// A new connection: start over, and check whether it has an avatar.
    fn connect(&mut self, client: Arc<ChatClient>) {
        self.generation += 1;
        self.flow = Flow::new();
        self.client = Some(Arc::clone(&client));
        let (generation, tx, waker) = (self.generation, self.tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            // A failed read counts as an avatar: an unrelated error must not
            // block the GUI on the loading screen.
            let has_avatar = match client.avatar_manifest().await {
                Ok(m) => m.config.vrm.is_some() || m.config.sprite.is_some(),
                Err(e) => {
                    tracing::warn!("cannot read the avatar manifest: {e:#}");
                    true
                }
            };
            let _ = tx.send((generation, Event::Checked { has_avatar }));
            waker();
        });
    }

    /// Sends `request` on the flow's connection.
    fn send(&self, request: Request) {
        let generation = self.generation;
        let Some(client) = self.client.clone() else {
            // Unreachable in practice: a request follows a check, which
            // needs a connection.
            let _ = self.tx.send((
                generation,
                Event::Answered(Err("not connected to the agent".to_owned())),
            ));
            (self.waker)();
            return;
        };
        let (tx, waker) = (self.tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let progress = {
                let (tx, waker) = (tx.clone(), Arc::clone(&waker));
                move |p| {
                    let _ = tx.send((generation, Event::Progress(p)));
                    waker();
                }
            };
            let answer = client
                .avatar_install_default(request.auto, progress)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send((generation, Event::Answered(answer)));
            waker();
        });
    }

    /// Folds the events of this connection into the flow. Returns whether
    /// to resync the avatar. `assets_generation` is the avatar's now, kept
    /// for the reload to wait past.
    fn drain(&mut self, assets_generation: u64) -> bool {
        let events: Vec<_> = match self.rx.lock() {
            Ok(mut rx) => std::iter::from_fn(|| rx.try_recv().ok()).collect(),
            Err(_) => return false,
        };
        let mut resync = false;
        for (generation, event) in events {
            if generation != self.generation {
                continue;
            }
            match event {
                Event::Checked { has_avatar } => {
                    if let Some(request) = self.flow.checked(has_avatar) {
                        self.send(request);
                    }
                }
                Event::Progress(p) => self.flow.progress(p),
                Event::Answered(answer) => {
                    if let Err(e) = &answer {
                        tracing::warn!("cannot install the default avatar: {e}");
                    }
                    if self.flow.answered(answer) {
                        self.reload_from = assets_generation;
                        resync = true;
                    }
                }
            }
        }
        resync
    }
}

/// Every frame, before the renderer is chosen: restarts the flow on a new
/// connection, folds in what the jobs reported, and asks for a resync once
/// the default avatar is installed.
pub(crate) fn drive_default_avatar(
    bridge: Res<ChatBridge>,
    assets: Option<Res<AvatarAssets>>,
    mut default_avatar: ResMut<DefaultAvatar>,
    mut sync: ResMut<SyncRequested>,
) {
    let live = bridge.client();
    if connection_changed(default_avatar.client.as_ref(), live.as_ref())
        && let Some(client) = live
    {
        default_avatar.connect(client);
    }
    if default_avatar.drain(assets.map_or(0, |a| a.generation)) {
        sync.0 = true;
    }
}

/// Every frame, after the finished syncs are folded in: leaves the loading
/// screen once the resync after the install has finished, in the frame it
/// lands.
pub(crate) fn finish_default_reload(
    assets: Option<Res<AvatarAssets>>,
    mut default_avatar: ResMut<DefaultAvatar>,
) {
    if *default_avatar.flow() != Flow::Reloading {
        return;
    }
    // Without the assets nothing will reload; do not wait for it.
    let finished = assets.is_none_or(|a| {
        resync_finished(default_avatar.reload_from, a.generation, a.error.is_some())
    });
    if finished {
        default_avatar.flow.reloaded();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked, no avatar: asked the agent, no answer yet.
    fn asking() -> Flow {
        let mut f = Flow::new();
        f.checked(false);
        f
    }

    /// Asked, and the agent started downloading.
    fn loading() -> Flow {
        let mut f = asking();
        f.progress(InstallProgress {
            received: 0,
            total: None,
        });
        f
    }

    #[test]
    fn an_avatar_present_needs_nothing() {
        let mut f = Flow::new();
        assert_eq!(f.checked(true), None);
        assert_eq!(f, Flow::Done { offer: false });
        assert!(!f.blocks_screen());
    }

    #[test]
    fn no_avatar_asks_without_blocking() {
        let mut f = Flow::new();
        assert_eq!(f.checked(false), Some(Request { auto: true }));
        assert_eq!(f, Flow::Asking);
        assert!(!f.blocks_screen());
    }

    #[test]
    fn the_first_progress_blocks() {
        let mut f = asking();
        f.progress(InstallProgress {
            received: 0,
            total: None,
        });
        assert_eq!(
            f,
            Flow::Loading {
                received: 0,
                total: None
            }
        );
        assert!(f.blocks_screen());
    }

    #[test]
    fn non_download_answers_from_asking_never_block() {
        let mut f = asking();
        assert!(!f.answered(Ok(DefaultAvatarAnswer::Unavailable)));
        assert_eq!(f, Flow::Done { offer: false });
        assert!(!f.blocks_screen());
        let mut f = asking();
        assert!(!f.answered(Ok(DefaultAvatarAnswer::NeedsConfirmation)));
        assert_eq!(f, Flow::Done { offer: true });
        assert!(!f.blocks_screen());
        let mut f = asking();
        assert!(f.answered(Ok(DefaultAvatarAnswer::AlreadyPresent)));
        assert_eq!(f, Flow::Done { offer: false });
        assert!(!f.blocks_screen());
    }

    #[test]
    fn an_error_from_asking_fails() {
        let mut f = asking();
        assert!(!f.answered(Err("offline".into())));
        assert_eq!(
            f,
            Flow::Failed {
                message: "offline".into()
            }
        );
        assert!(f.blocks_screen());
    }

    #[test]
    fn installed_reloads_before_unblocking() {
        let mut f = loading();
        assert!(f.answered(Ok(DefaultAvatarAnswer::Installed)));
        assert_eq!(f, Flow::Reloading);
        assert!(f.blocks_screen());
        f.reloaded();
        assert_eq!(f, Flow::Done { offer: false });
        assert!(!f.blocks_screen());
        let mut g = loading();
        assert!(g.answered(Ok(DefaultAvatarAnswer::AlreadyPresent)));
        assert_eq!(g, Flow::Reloading);
    }

    #[test]
    fn reloaded_only_while_reloading() {
        let mut f = loading();
        f.reloaded();
        assert!(matches!(f, Flow::Loading { .. }));
    }

    #[test]
    fn the_resync_finishes_on_a_new_generation_or_an_error() {
        assert!(!resync_finished(3, 3, false));
        assert!(resync_finished(3, 4, false));
        assert!(resync_finished(3, 3, true));
    }

    #[test]
    fn needs_confirmation_offers_the_button() {
        let mut f = loading();
        assert!(!f.answered(Ok(DefaultAvatarAnswer::NeedsConfirmation)));
        assert_eq!(f, Flow::Done { offer: true });
        assert_eq!(f.get(), Some(Request { auto: false }));
        assert!(matches!(f, Flow::Loading { .. }));
    }

    #[test]
    fn unavailable_offers_nothing() {
        let mut f = loading();
        assert!(!f.answered(Ok(DefaultAvatarAnswer::Unavailable)));
        assert_eq!(f, Flow::Done { offer: false });
        assert_eq!(f.get(), None);
    }

    #[test]
    fn failure_blocks_until_retry_or_continue() {
        let mut f = loading();
        assert!(!f.answered(Err("offline".into())));
        assert_eq!(
            f,
            Flow::Failed {
                message: "offline".into()
            }
        );
        assert!(f.blocks_screen());
        assert_eq!(f.retry(), Some(Request { auto: false }));
        assert!(matches!(f, Flow::Loading { .. }));
        f.answered(Err("offline".into()));
        f.continue_without();
        assert_eq!(f, Flow::Done { offer: true });
        assert!(!f.blocks_screen());
    }

    #[test]
    fn no_automatic_retry_after_continuing() {
        let mut f = loading();
        f.answered(Err("offline".into()));
        f.continue_without();
        assert_eq!(f.checked(false), None);
        assert_eq!(f, Flow::Done { offer: true });
    }

    #[test]
    fn progress_only_while_loading() {
        let p = InstallProgress {
            received: 5,
            total: Some(10),
        };
        let mut f = Flow::Done { offer: false };
        f.progress(p);
        assert_eq!(f, Flow::Done { offer: false });
        let mut f = loading();
        f.progress(p);
        assert_eq!(
            f,
            Flow::Loading {
                received: 5,
                total: Some(10)
            }
        );
    }

    #[test]
    fn a_new_client_restarts_the_check() {
        use std::sync::Arc;
        let (a, b) = (Arc::new(1), Arc::new(1));
        assert!(connection_changed(None, Some(&a)), "the first client");
        assert!(!connection_changed(Some(&a), Some(&a)), "the same client");
        assert!(connection_changed(Some(&a), Some(&b)), "a reconnect");
        assert!(!connection_changed(Some(&a), None), "disconnected");
        assert!(!connection_changed::<i32>(None, None));
    }
}
