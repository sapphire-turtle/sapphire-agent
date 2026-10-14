//! Standing answers to `session/request_permission`, per room profile.
//!
//! Kept beside the host-local config rather than in the workspace. The
//! workspace is a synced artefact, and "always allow `file_write` for
//! this editor" is a statement about *this machine's* trust in *this
//! client* — the same category as the credentials, bind addresses and
//! machine paths `main.rs` already keeps host-local.
//!
//! Grants are per tool name. Argument-level grants ("always, for paths
//! under this directory") would need a path-normalisation design and
//! are deliberately out of scope.

use crate::tools::policy::Approval;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::warn;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileGrants>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileGrants {
    #[serde(default)]
    always_allow: BTreeSet<String>,
    #[serde(default)]
    always_reject: BTreeSet<String>,
}

/// The recorded answers, and the file they live in.
pub(crate) struct PermissionStore {
    path: PathBuf,
    /// A blocking mutex: every critical section here is a map lookup or
    /// a small synchronous write, never held across an await.
    state: Mutex<Persisted>,
}

impl PermissionStore {
    /// `~/.config/sapphire-agent/acp-permissions.json`, resolved the
    /// way `Config::default_path` resolves the config file itself.
    pub(crate) fn default_path() -> PathBuf {
        if let Some(dirs) = directories::ProjectDirs::from("", "", "sapphire-agent") {
            dirs.config_dir().join("acp-permissions.json")
        } else {
            PathBuf::from("acp-permissions.json")
        }
    }

    /// Load what is on disk. Never fails: a missing file is an empty
    /// record, and an unreadable one is logged and treated as empty.
    /// Losing the grants means asking again, which is the safe
    /// direction; refusing to start the agent over it is not.
    pub(crate) fn open(path: PathBuf) -> Self {
        let state = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Persisted>(&bytes) {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!(
                        "ACP: ignoring unreadable permission record at {}: {e}. \
                         Standing answers are lost; the client will be asked again.",
                        path.display()
                    );
                    Persisted::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(e) => {
                warn!(
                    "ACP: could not read the permission record at {}: {e}",
                    path.display()
                );
                Persisted::default()
            }
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// `Some(true)` = always allow, `Some(false)` = always reject,
    /// `None` = nothing recorded, so ask.
    ///
    /// Reject wins over allow: a tool somehow present in both lists is
    /// refused, because that is the safe side of a contradiction.
    pub(crate) fn standing(&self, profile: &str, tool: &str) -> Option<bool> {
        let state = self.state.lock().expect("permission store poisoned");
        let grants = state.profiles.get(profile)?;
        if grants.always_reject.contains(tool) {
            Some(false)
        } else if grants.always_allow.contains(tool) {
            Some(true)
        } else {
            None
        }
    }

    /// Record an answer. One-off answers are dropped: only the
    /// `Always` variants are meant to outlive the call.
    pub(crate) fn record(&self, profile: &str, tool: &str, approval: Approval) {
        if !approval.is_sticky() {
            return;
        }
        {
            let mut state = self.state.lock().expect("permission store poisoned");
            let grants = state.profiles.entry(profile.to_string()).or_default();
            // The newest answer replaces the older one, so remove from
            // both lists before inserting into one.
            grants.always_allow.remove(tool);
            grants.always_reject.remove(tool);
            if approval.allows() {
                grants.always_allow.insert(tool.to_string());
            } else {
                grants.always_reject.insert(tool.to_string());
            }
        }
        self.flush();
    }

    /// Write the whole record out. Temp file then rename, so a crash
    /// mid-write cannot leave a half-written record that the next start
    /// would discard entirely.
    fn flush(&self) {
        // The lock is held across the write and the rename, not just the
        // serialisation. Two connections recording an answer at once
        // would otherwise both write the same `.json.tmp` and rename it,
        // and the interleaving can publish a half-written file. Holding
        // it costs nothing — these are small synchronous writes, and the
        // critical section still never spans an await.
        let state = self.state.lock().expect("permission store poisoned");
        let json = match serde_json::to_vec_pretty(&*state) {
            Ok(v) => v,
            Err(e) => {
                warn!("ACP: could not serialise the permission record: {e}");
                return;
            }
        };

        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                "ACP: could not create {} for the permission record: {e}",
                parent.display()
            );
            return;
        }

        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!("ACP: could not write {}: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            warn!(
                "ACP: could not replace {} with {}: {e}",
                self.path.display(),
                tmp.display()
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::policy::Approval;

    fn temp_store() -> (tempfile::TempDir, PermissionStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp-permissions.json");
        let store = PermissionStore::open(path);
        (dir, store)
    }

    /// Nothing recorded yet means "no standing answer" — ask.
    #[test]
    fn an_unknown_tool_has_no_standing_answer() {
        let (_dir, store) = temp_store();
        assert_eq!(store.standing("zed", "file_write"), None);
    }

    /// Only the `Always` variants stick. A one-off answer must not
    /// silently become permanent.
    #[test]
    fn once_answers_are_not_recorded() {
        let (_dir, store) = temp_store();
        store.record("zed", "file_write", Approval::AllowOnce);
        store.record("zed", "file_delete", Approval::RejectOnce);
        assert_eq!(store.standing("zed", "file_write"), None);
        assert_eq!(store.standing("zed", "file_delete"), None);
    }

    #[test]
    fn always_answers_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp-permissions.json");

        let store = PermissionStore::open(path.clone());
        store.record("zed", "file_write", Approval::AllowAlways);
        store.record("zed", "file_delete", Approval::RejectAlways);

        // A fresh process would see this.
        let reopened = PermissionStore::open(path);
        assert_eq!(reopened.standing("zed", "file_write"), Some(true));
        assert_eq!(reopened.standing("zed", "file_delete"), Some(false));
    }

    /// Grants are per room profile: a token pinned to one profile must
    /// not inherit another profile's standing answers.
    #[test]
    fn profiles_do_not_share_grants() {
        let (_dir, store) = temp_store();
        store.record("zed", "shell", Approval::AllowAlways);
        assert_eq!(store.standing("zed", "shell"), Some(true));
        assert_eq!(store.standing("matrix", "shell"), None);
    }

    /// Reject wins. A tool listed in both must not run — the safe side
    /// of a contradiction is refusal.
    #[test]
    fn reject_takes_precedence_over_allow() {
        let (_dir, store) = temp_store();
        // Reach past `record`, which keeps the two lists disjoint, to
        // build the contradiction `standing` has to resolve. A record
        // hand-edited or written by an older version can look like this.
        {
            let mut state = store.state.lock().unwrap();
            let grants = state.profiles.entry("zed".to_string()).or_default();
            grants.always_allow.insert("shell".to_string());
            grants.always_reject.insert("shell".to_string());
        }
        assert_eq!(store.standing("zed", "shell"), Some(false));
    }

    /// A grant that is later revoked stops applying.
    #[test]
    fn a_later_answer_replaces_an_earlier_one() {
        let (_dir, store) = temp_store();
        store.record("zed", "file_write", Approval::RejectAlways);
        assert_eq!(store.standing("zed", "file_write"), Some(false));
        store.record("zed", "file_write", Approval::AllowAlways);
        assert_eq!(store.standing("zed", "file_write"), Some(true));
    }

    /// An unreadable record is not a reason to refuse to start. It is
    /// treated as empty, which means "ask" — the safe direction.
    #[test]
    fn a_corrupt_file_is_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp-permissions.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = PermissionStore::open(path);
        assert_eq!(store.standing("zed", "file_write"), None);

        // And it must still be writable afterwards.
        store.record("zed", "file_write", Approval::AllowAlways);
        assert_eq!(store.standing("zed", "file_write"), Some(true));
    }

    /// The directory may not exist yet on a fresh host.
    #[test]
    fn a_missing_directory_is_created_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("acp-permissions.json");

        let store = PermissionStore::open(path.clone());
        store.record("zed", "file_write", Approval::AllowAlways);

        assert!(path.exists(), "the record file should have been created");
        assert_eq!(
            PermissionStore::open(path).standing("zed", "file_write"),
            Some(true)
        );
    }

    /// #270 renamed the names the client-side file and shell tools used,
    /// and the ruling was that the standing answers do **not** follow
    /// them: `always_allow` is keyed by tool name, an old name's grant is
    /// a dead entry, and the new name is asked once.
    ///
    /// The reason is that the rename is not a rename: `client_file_read`
    /// and `file_read` reach different machines depending on the session,
    /// so carrying a grant over would widen a permission the user gave
    /// for something they were looking at when they gave it. There is
    /// deliberately no migration code — the assertion that the old keys
    /// are still on disk, untouched, is what would fail if someone added
    /// one.
    #[test]
    fn pre_270_grants_do_not_transfer_to_the_unified_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp-permissions.json");

        let store = PermissionStore::open(path.clone());
        store.record("zed", "client_file_read", Approval::AllowAlways);
        store.record("zed", "client_shell", Approval::AllowAlways);
        assert_eq!(store.standing("zed", "client_file_read"), Some(true));

        // What the next process — i.e. the upgraded agent — sees.
        let reopened = PermissionStore::open(path.clone());
        assert_eq!(
            reopened.standing("zed", "file_read"),
            None,
            "a grant for the old name must not settle the new one"
        );
        assert_eq!(reopened.standing("zed", "shell"), None);

        // And nothing rewrote or pruned the record on the way through.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["profiles"]["zed"]["always_allow"],
            serde_json::json!(["client_file_read", "client_shell"]),
            "the old keys stay as dead entries — no migration, no cleanup"
        );

        // The new name is asked once and then sticks, like any other.
        reopened.record("zed", "file_read", Approval::AllowAlways);
        assert_eq!(
            PermissionStore::open(path).standing("zed", "file_read"),
            Some(true)
        );
    }
}
