//! The framework's app server, around the agent.
//!
//! `sapphire-agent` runs as a sapphire-framework application: the
//! framework's [`AppServer`] owns the IPC endpoint, the current workspace
//! (`workspace init|select|show|…`), its search index and the process's
//! signals, and the framework's verbs (`status`, `service`, `workspace`,
//! `workgroup`, `device`, `embedding`, `external-device`) are this binary's
//! too. See `docs/superpowers/specs/2026-10-13-app-server-sync-acp-stdio-design.md`.
//!
//! What the agent adds on top — channels, the HTTP API, the agent loop — is
//! built in `main` around the workspace this server has open, never around
//! one of its own: the cache is a redb database, which only one process may
//! open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use sapphire_framework::ipc::ManagedBy;
use sapphire_framework::server::{AppServer, Selection, SelectionFile, SyncRuntime, WorkspaceHost};
use sapphire_framework::workspace::WorkspaceState;
use sapphire_framework_bridge_client::BridgeClient;
use tokio_util::sync::CancellationToken;

use crate::APP_CTX;

/// This binary's version, reported in every handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The exit code after a workspace switch: `EX_TEMPFAIL`. The service units
/// the framework installs restart on failure, so a non-zero code is what
/// brings the agent back up around the new workspace.
pub const RESTART_EXIT_CODE: i32 = 75;

/// How often the switch watcher compares the server's workspace with the
/// one the agent was built around.
const SWITCH_POLL: Duration = Duration::from_secs(2);

/// The framework's selected workspace, if any.
pub fn selection() -> Result<Option<Selection>> {
    SelectionFile::for_app(&APP_CTX)
        .load(APP_CTX.app_name)
        .context("reading the selected workspace")
}

/// Make `dir` the selected workspace: what an agent configured by
/// `workspace_dir` alone becomes on its first start under the app server.
///
/// The framework only selects a directory that holds the app's marker
/// (`.sapphire-agent/`). The agent's workspace config already lives there,
/// so creating it is no more than the agent would do itself. Sync stays
/// off; `workspace sync enable` turns it on.
pub fn adopt(dir: &Path) -> Result<PathBuf> {
    adopt_into(&SelectionFile::for_app(&APP_CTX), dir)
}

fn adopt_into(file: &SelectionFile, dir: &Path) -> Result<PathBuf> {
    let marker = dir.join(format!(".{}", APP_CTX.app_name));
    std::fs::create_dir_all(&marker)
        .with_context(|| format!("creating the workspace marker {}", marker.display()))?;
    let root = dir
        .canonicalize()
        .with_context(|| format!("resolving the workspace {}", dir.display()))?;
    file.save(&Selection {
        root: root.clone(),
        sync: false,
    })
    .context("recording the selected workspace")?;
    Ok(root)
}

/// The server the framework's one-shot verbs dispatch against. `status`,
/// `workspace`, `external-device` and the rest read only the app name and
/// the service spec from it and talk to the running server or the bridge
/// themselves.
pub fn build() -> AppServer {
    AppServer::new(&APP_CTX, VERSION).app_api(crate::serve::acp_tunnel::APP_API)
}

/// Attach sync, through the host's `sapphire-bridge` when one is running.
///
/// A bridge that is not running is not fatal — the agent serves its
/// workspace without sync, and picks the bridge up on its next start — but
/// any other failure (a bridge of another version, say) stops the start.
/// Returns the runtime and the bridge for `placement` to ask.
pub async fn with_sync(
    server: AppServer,
) -> Result<(AppServer, Option<(Arc<SyncRuntime>, Arc<BridgeClient>)>)> {
    match BridgeClient::connect(APP_CTX.app_name, VERSION).await {
        Ok(bridge) => {
            let bridge = Arc::new(bridge);
            let runtime = Arc::new(SyncRuntime::new(
                &APP_CTX,
                Arc::clone(&bridge),
                std::env::current_exe().context("locating this executable")?,
                ManagedBy::Service,
            ));
            Ok((server.sync(Arc::clone(&runtime)), Some((runtime, bridge))))
        }
        Err(sapphire_framework::ipc::Error::NotRunning(reason)) => {
            tracing::warn!("{reason}; serving without sync until the agent is restarted");
            Ok((server, None))
        }
        Err(err) => Err(err).context("connecting to the sapphire bridge"),
    }
}

/// The workspace the running server has open, once it has one.
///
/// [`AppServer::run`] restores the selection as it starts, so this waits
/// for that rather than opening anything itself. With nothing selected it
/// keeps waiting — `workspace init` or `select` against the running server
/// is how one gets chosen — and says so once.
pub async fn open_workspace(host: &WorkspaceHost) -> Result<(PathBuf, Arc<WorkspaceState>)> {
    let mut said = false;
    let started = tokio::time::Instant::now();
    loop {
        if host.root().is_some() {
            let (root, backend) = host
                .current()
                .await
                .context("opening the selected workspace")?;
            return Ok((root, Arc::clone(backend.state())));
        }
        if !said && started.elapsed() > Duration::from_secs(3) {
            tracing::warn!(
                "no workspace is selected; run `{0} workspace init <dir>` or `{0} workspace \
                 select <dir>` to choose one",
                APP_CTX.app_name
            );
            said = true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stop the agent when the server switches to another workspace.
///
/// Everything the agent holds — sessions, memory, tools, the bootstrap
/// files — is rooted in the workspace it was built around, so a switch is a
/// restart (design decision 2): this cancels `shutdown` and sets
/// `restart`, and `main` exits with [`RESTART_EXIT_CODE`] once the agent
/// has wound down.
pub fn watch_for_switch(
    host: Arc<WorkspaceHost>,
    root: PathBuf,
    shutdown: CancellationToken,
    restart: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(SWITCH_POLL) => {}
            }
            let now = host.root();
            if now.as_deref() != Some(root.as_path()) {
                tracing::info!(
                    "the server switched from {} to {}; restarting the agent around it",
                    root.display(),
                    now.as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "no workspace".to_owned())
                );
                restart.store(true, Ordering::SeqCst);
                shutdown.cancel();
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopting_a_directory_marks_and_selects_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notes");
        std::fs::create_dir_all(&dir).unwrap();
        let file = SelectionFile::at(tmp.path().join("workspace.toml"));

        let root = adopt_into(&file, &dir).unwrap();

        assert!(
            dir.join(".sapphire-agent").is_dir(),
            "the marker the framework selects by"
        );
        let selected = file.load(APP_CTX.app_name).unwrap().expect("a selection");
        assert_eq!(selected.root, root);
        assert_eq!(root, dir.canonicalize().unwrap());
        assert!(!selected.sync, "sync stays off until asked for");
    }

    #[test]
    fn adopting_keeps_an_existing_marker_and_its_config() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join(".sapphire-agent");
        std::fs::create_dir_all(&marker).unwrap();
        std::fs::write(marker.join("config.toml"), "x = 1\n").unwrap();
        let file = SelectionFile::at(tmp.path().join("sel.toml"));

        adopt_into(&file, tmp.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(marker.join("config.toml")).unwrap(),
            "x = 1\n"
        );
    }

    /// A host that stops serving the agent's workspace — closed here, the
    /// same observable as a switch — stops the agent and asks for a restart.
    #[tokio::test]
    async fn a_workspace_that_goes_away_stops_the_agent_for_a_restart() {
        let host = Arc::new(WorkspaceHost::new(&APP_CTX));
        let shutdown = CancellationToken::new();
        let restart = Arc::new(AtomicBool::new(false));
        let watch = watch_for_switch(
            Arc::clone(&host),
            PathBuf::from("/the/agents/workspace"),
            shutdown.clone(),
            Arc::clone(&restart),
        );

        tokio::time::timeout(Duration::from_secs(10), shutdown.cancelled())
            .await
            .expect("the watcher stops the agent");
        assert!(restart.load(Ordering::SeqCst));
        watch.await.unwrap();
    }

    #[tokio::test]
    async fn the_watcher_ends_quietly_with_the_agent() {
        let host = Arc::new(WorkspaceHost::new(&APP_CTX));
        let shutdown = CancellationToken::new();
        let restart = Arc::new(AtomicBool::new(false));
        let watch = watch_for_switch(
            host,
            PathBuf::from("/x"),
            shutdown.clone(),
            Arc::clone(&restart),
        );
        shutdown.cancel();
        watch.await.unwrap();
        assert!(
            !restart.load(Ordering::SeqCst),
            "a shutdown is not a switch"
        );
    }
}
