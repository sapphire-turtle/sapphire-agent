//! A file picker the host app provides to renderer panels.
//!
//! Choosing a file is the platform's to do (a native dialog on the
//! desktop, a document picker on a phone), so this crate only holds the
//! seam: the host installs a [`FileDialog`] with
//! [`FilePickerApp::set_file_dialog`], and panels ask [`FilePicker`] for a
//! file by ticket, the way they ask [`AvatarOps`](crate::avatar::AvatarOps)
//! for an edit: [`FilePicker::pick`] returns at once, the dialog runs on
//! the GUI's runtime, and the panel reads the outcome with
//! [`FilePicker::result`] on a later frame. The GUI is woken when the
//! dialog closes, so an idle window shows the outcome.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bevy::prelude::*;
use tokio::runtime::Handle;

use crate::Waker;

/// The kind of file a dialog should offer, as the dialog's file-type
/// filter: `extensions` are lowercase and without the dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFilter {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
}

/// A file the user picked, read into memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedFile {
    /// The file's name, without its directory.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What [`FileDialog::pick`] resolves to: `Ok(None)` when the user
/// cancelled, `Err` with a message fit to show when the dialog or the
/// read failed.
pub type PickOutcome = Result<Option<PickedFile>, String>;

/// A boxed future, so the trait needs no futures crate.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The host's file dialog. It runs on the GUI's tokio runtime, off the
/// main thread, so an implementation must use its platform's async dialog.
pub trait FileDialog: Send + Sync {
    /// Shows a dialog titled `title` that offers files matching `filters`
    /// (any file when empty), and reads the one the user picks.
    fn pick(&self, title: String, filters: Vec<FileFilter>) -> BoxFuture<PickOutcome>;
}

/// Names one [`FilePicker::pick`], to read its outcome with
/// [`FilePicker::result`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickTicket(u64);

/// How many outcomes nobody has taken are remembered.
const KEPT_RESULTS: usize = 16;

#[derive(Default)]
struct Inner {
    next_ticket: u64,
    results: BTreeMap<u64, PickOutcome>,
}

/// Records a pick's end however the task ends: a panicking dialog still
/// reports a failure.
struct Finish {
    picker: FilePicker,
    ticket: PickTicket,
    outcome: Option<PickOutcome>,
}

impl Finish {
    /// How the pick ended; recorded when `self` drops.
    fn set(&mut self, outcome: PickOutcome) {
        self.outcome = Some(outcome);
    }
}

impl Drop for Finish {
    fn drop(&mut self) {
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| Err("the file dialog stopped unexpectedly".to_owned()));
        self.picker.finish(self.ticket, outcome);
    }
}

/// Lets renderer panels ask the user for a file. [`PanelContext`] hands
/// panels one; the host installs the dialog behind it.
///
/// [`PanelContext`]: super::character_page::PanelContext
#[derive(Resource, Clone)]
pub struct FilePicker {
    dialog: Option<Arc<dyn FileDialog>>,
    runtime: Handle,
    waker: Waker,
    inner: Arc<Mutex<Inner>>,
}

impl FilePicker {
    /// A picker running `dialog` (if the host has one) on `runtime`,
    /// waking the GUI with `waker` when a dialog closes.
    pub fn new(dialog: Option<Arc<dyn FileDialog>>, runtime: Handle, waker: Waker) -> Self {
        Self {
            dialog,
            runtime,
            waker,
            inner: Arc::default(),
        }
    }

    /// Whether the host has a file dialog. A panel may hide its "choose a
    /// file" button without one; [`Self::pick`] then reports an error.
    pub fn available(&self) -> bool {
        self.dialog.is_some()
    }

    /// Opens the dialog and returns at once; read the outcome with
    /// [`Self::result`]. Without a dialog the outcome is an error already.
    pub fn pick(&self, title: impl Into<String>, filters: Vec<FileFilter>) -> PickTicket {
        let ticket = {
            let mut inner = self.lock();
            let ticket = PickTicket(inner.next_ticket);
            inner.next_ticket += 1;
            ticket
        };
        let mut finish = Finish {
            picker: self.clone(),
            ticket,
            outcome: None,
        };
        let Some(dialog) = self.dialog.clone() else {
            finish.set(Err("no file dialog on this platform".to_owned()));
            return ticket;
        };
        let title = title.into();
        self.runtime.spawn(async move {
            finish.set(dialog.pick(title, filters).await);
        });
        ticket
    }

    /// How the pick `ticket` names ended, taken: `None` while the dialog
    /// is open, and again once the outcome has been read.
    pub fn result(&self, ticket: &PickTicket) -> Option<PickOutcome> {
        self.lock().results.remove(&ticket.0)
    }

    fn finish(&self, ticket: PickTicket, outcome: PickOutcome) {
        {
            let mut inner = self.lock();
            inner.results.insert(ticket.0, outcome);
            // A panel that never reads its ticket must not grow this
            // forever.
            while inner.results.len() > KEPT_RESULTS {
                inner.results.pop_first();
            }
        }
        (self.waker)();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The dialog a host installed before [`crate::AgentGuiPlugin`] finished,
/// waiting for the runtime and waker the picker also needs.
#[derive(Resource, Default)]
pub(crate) struct InstalledDialog(Mutex<Option<Arc<dyn FileDialog>>>);

impl InstalledDialog {
    pub(crate) fn take(&self) -> Option<Arc<dyn FileDialog>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Installs the host's [`FileDialog`] on an [`App`].
pub trait FilePickerApp {
    /// Makes `dialog` the one [`FilePicker`] shows. Safe in any plugin
    /// order: the holder is created if it is not there yet, and the picker
    /// is built when [`crate::AgentGuiPlugin`] finishes.
    fn set_file_dialog(&mut self, dialog: Arc<dyn FileDialog>) -> &mut Self;
}

impl FilePickerApp for App {
    fn set_file_dialog(&mut self, dialog: Arc<dyn FileDialog>) -> &mut Self {
        *self
            .world_mut()
            .get_resource_or_init::<InstalledDialog>()
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(dialog);
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    /// Answers by the dialog's title, after the delay the title's number
    /// asks for ("slow" waits).
    struct Fake;

    impl FileDialog for Fake {
        fn pick(&self, title: String, _filters: Vec<FileFilter>) -> BoxFuture<PickOutcome> {
            Box::pin(async move {
                match title.as_str() {
                    "cancel" => Ok(None),
                    "slow" => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(Some(PickedFile {
                            name: "slow.png".into(),
                            bytes: vec![2],
                        }))
                    }
                    "fail" => Err("portal unavailable".into()),
                    "panic" => panic!("dialog crashed"),
                    _ => Ok(Some(PickedFile {
                        name: "a.png".into(),
                        bytes: vec![1],
                    })),
                }
            })
        }
    }

    fn picker(
        dialog: Option<Arc<dyn FileDialog>>,
    ) -> (FilePicker, tokio::runtime::Runtime, Arc<AtomicUsize>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&wakes);
        let waker: Waker = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        (
            FilePicker::new(dialog, runtime.handle().clone(), waker),
            runtime,
            wakes,
        )
    }

    /// Polls `ticket` until its outcome arrives.
    fn wait(picker: &FilePicker, ticket: &PickTicket) -> PickOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = picker.result(ticket) {
                return outcome;
            }
            assert!(Instant::now() < deadline, "the pick never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_pick_resolves_once_by_ticket() {
        let (picker, _rt, wakes) = picker(Some(Arc::new(Fake)));
        assert!(picker.available());
        let ticket = picker.pick("any", vec![]);
        let picked = wait(&picker, &ticket).unwrap().unwrap();
        assert_eq!(picked.name, "a.png");
        assert_eq!(picked.bytes, vec![1]);
        assert_eq!(picker.result(&ticket), None, "taken once");
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "the GUI is woken once");
    }

    #[test]
    fn cancel_is_ok_none() {
        let (picker, _rt, _) = picker(Some(Arc::new(Fake)));
        let ticket = picker.pick("cancel", vec![]);
        assert_eq!(wait(&picker, &ticket), Ok(None));
    }

    #[test]
    fn two_picks_keep_their_own_results() {
        let (picker, _rt, _) = picker(Some(Arc::new(Fake)));
        let slow = picker.pick("slow", vec![]);
        let cancelled = picker.pick("cancel", vec![]);
        assert_eq!(wait(&picker, &cancelled), Ok(None));
        assert_eq!(picker.result(&slow), None, "still open");
        assert_eq!(wait(&picker, &slow).unwrap().unwrap().name, "slow.png");
    }

    #[test]
    fn a_failing_or_panicking_dialog_is_an_error() {
        let (picker, _rt, _) = picker(Some(Arc::new(Fake)));
        let failed = picker.pick("fail", vec![]);
        assert_eq!(wait(&picker, &failed), Err("portal unavailable".into()));
        let panicked = picker.pick("panic", vec![]);
        assert!(wait(&picker, &panicked).is_err());
    }

    #[test]
    fn no_dialog_reports_unavailable() {
        let (picker, _rt, wakes) = picker(None);
        assert!(!picker.available());
        let ticket = picker.pick("any", vec![]);
        let err = picker.result(&ticket).unwrap().unwrap_err();
        assert!(err.contains("no file dialog"), "{err}");
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_dialog_set_in_any_plugin_order_is_picked_up() {
        let mut app = App::new();
        assert!(app.world().get_resource::<InstalledDialog>().is_none());
        app.set_file_dialog(Arc::new(Fake));
        let installed = app.world().resource::<InstalledDialog>();
        assert!(installed.take().is_some());
        assert!(installed.take().is_none(), "taken once");
    }
}
