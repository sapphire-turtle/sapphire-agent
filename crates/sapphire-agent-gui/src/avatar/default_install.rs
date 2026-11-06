//! The first-run flow for the default avatar, as a pure state machine: no
//! bevy, no IO. The caller feeds it what happened (the manifest was checked,
//! progress arrived, the agent answered, the user pressed a button) and sends
//! the [`Request`] it returns.

use sapphire_agent_core::{DefaultAvatarAnswer, InstallProgress};

/// Where the first-run flow stands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Flow {
    /// Connected, the manifest not yet checked.
    Checking,
    /// The agent is downloading the default avatar.
    Loading { received: u64, total: Option<u64> },
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
            self.start_loading(true)
        }
    }

    /// Download progress. Applies only while `Loading`, so a stale one after
    /// the answer cannot reopen the loading screen.
    pub(crate) fn progress(&mut self, p: InstallProgress) {
        if let Flow::Loading { received, total } = self {
            *received = p.received;
            *total = p.total;
        }
    }

    /// The agent answered (or the call failed). Returns whether to resync the
    /// avatar now. Applies only while `Loading`.
    pub(crate) fn answered(&mut self, a: Result<DefaultAvatarAnswer, String>) -> bool {
        if !matches!(self, Flow::Loading { .. }) {
            return false;
        }
        match a {
            Ok(DefaultAvatarAnswer::Installed | DefaultAvatarAnswer::AlreadyPresent) => {
                *self = Flow::Done { offer: false };
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
        matches!(self, Flow::Loading { .. } | Flow::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loading() -> Flow {
        let mut f = Flow::new();
        f.checked(false);
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
    fn no_avatar_loads_automatically() {
        let mut f = Flow::new();
        assert_eq!(f.checked(false), Some(Request { auto: true }));
        assert!(f.blocks_screen());
    }

    #[test]
    fn installed_resyncs_and_unblocks() {
        let mut f = loading();
        assert!(f.answered(Ok(DefaultAvatarAnswer::Installed)));
        assert_eq!(f, Flow::Done { offer: false });
        let mut g = loading();
        assert!(g.answered(Ok(DefaultAvatarAnswer::AlreadyPresent)));
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
}
