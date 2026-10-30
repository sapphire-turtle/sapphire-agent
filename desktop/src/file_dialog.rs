//! The system's file dialog, behind the GUI's [`FileDialog`] seam.
//!
//! Uses rfd's async dialog, which the GUI's tokio runtime drives from one
//! of its worker threads. rfd documents that on macOS the sync dialogs
//! must be created on the main thread (and the async ones then run on the
//! main thread's own loop internally), and on Windows a dialog is
//! hosted by the calling thread; the async API takes care of both, so
//! this is the form to use off the main thread. On Linux it asks the
//! desktop's file chooser portal over D-Bus, so there is no GTK to link.

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
            let mut dialog = AsyncFileDialog::new().set_title(title);
            for filter in filters {
                dialog = dialog.add_filter(filter.name, filter.extensions);
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
    fn the_name_has_no_directory() {
        assert_eq!(file_name(Path::new("/a/b/c.png")), "c.png");
        assert_eq!(file_name(Path::new("c.png")), "c.png");
    }
}
