//! The system's file dialog, behind the GUI's [`FileDialog`] seam.
//!
//! Uses rfd's async dialog, which the GUI's tokio runtime drives from one
//! of its worker threads. rfd documents that on macOS the sync dialogs
//! must be created on the main thread; the async API takes care of that,
//! so it is the form to use off the main thread. On macOS rfd shows a
//! sheet on the companion window when one exists (untested; for the human
//! check). On Windows a dialog is hosted by the calling thread.
//!
//! On Linux rfd asks the desktop's file chooser portal over D-Bus, so there
//! is no GTK to link, and falls back to `zenity` when the portal call
//! fails. When neither works rfd merely logs and resolves to "nothing
//! picked", which would look like a cancel; [`backend_missing`] checks for
//! that beforehand and the pick is then an error.

use std::path::Path;

use rfd::AsyncFileDialog;
use sapphire_agent_core::AVATAR_MAX_FILE;
use sapphire_agent_gui::companion::file_pick::{
    BoxFuture, FileDialog, FileFilter, PickOutcome, PickedFile,
};

pub struct RfdDialog;

impl FileDialog for RfdDialog {
    fn pick(&self, title: String, filters: Vec<FileFilter>) -> BoxFuture<PickOutcome> {
        Box::pin(async move {
            if let Some(problem) = backend_missing() {
                return Err(problem);
            }
            let mut dialog = AsyncFileDialog::new().set_title(title);
            for filter in filters {
                dialog = dialog.add_filter(filter.name, &with_uppercase(filter.extensions));
            }
            let Some(file) = dialog.pick_file().await else {
                return Ok(None);
            };
            // Before reading: a file past the limit is refused unread.
            let path = file.path();
            let size = std::fs::metadata(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?
                .len();
            check_size(size)?;
            let bytes = file.read().await;
            // Re-checked on what was read, in case the file grew.
            check_size(bytes.len() as u64)?;
            Ok(Some(PickedFile {
                name: file_name(path),
                bytes,
            }))
        })
    }
}

/// `extensions` and their uppercase forms: the portal matches extensions
/// case-sensitively, and avatar files are often `.PNG`.
fn with_uppercase(extensions: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = extensions.iter().map(|e| (*e).to_owned()).collect();
    for e in extensions {
        let upper = e.to_uppercase();
        if !all.contains(&upper) {
            all.push(upper);
        }
    }
    all
}

/// Why no dialog can be shown, when that is certain enough to say: on
/// Linux, no session bus for the portal and no `zenity` either.
#[cfg(target_os = "linux")]
fn backend_missing() -> Option<String> {
    let bus = std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| Path::new(&dir).join("bus").exists())
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let zenity = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("zenity").is_file()));
    (!bus && !zenity).then(|| "no file dialog: install xdg-desktop-portal or zenity".to_owned())
}

#[cfg(not(target_os = "linux"))]
fn backend_missing() -> Option<String> {
    None
}

fn check_size(size: u64) -> Result<(), String> {
    if size > AVATAR_MAX_FILE {
        return Err(format!(
            "the file is {size} bytes; an avatar file may be at most {AVATAR_MAX_FILE} bytes ({} MiB)",
            AVATAR_MAX_FILE / (1024 * 1024)
        ));
    }
    Ok(())
}

/// The file's name without its directory.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_over_the_limit_is_refused_naming_it() {
        assert!(check_size(AVATAR_MAX_FILE).is_ok());
        let err = check_size(AVATAR_MAX_FILE + 1).unwrap_err();
        assert!(err.contains(&AVATAR_MAX_FILE.to_string()), "{err}");
    }

    #[test]
    fn extensions_gain_their_uppercase_forms_once() {
        assert_eq!(
            with_uppercase(&["png", "PNG", "vrm"]),
            ["png", "PNG", "vrm", "VRM"]
        );
    }

    #[test]
    fn the_name_has_no_directory() {
        assert_eq!(file_name(Path::new("/a/b/c.png")), "c.png");
        assert_eq!(file_name(Path::new("c.png")), "c.png");
    }
}
