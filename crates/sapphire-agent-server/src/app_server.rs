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
//! built in `serve` around the workspace this server has open, never around
//! one of its own: the cache is a redb database, which only one process may
//! open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use sapphire_framework::ipc::ManagedBy;
use sapphire_framework::server::{
    AppServer, BridgeConnector, ReleaseSource, Selection, SelectionFile, SyncRuntime, WorkspaceHost,
};
use sapphire_framework::workspace::WorkspaceState;
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
pub fn build(edition: Edition) -> AppServer {
    AppServer::new(&APP_CTX, VERSION)
        .releases(edition.releases())
        .app_api(crate::serve::acp_tunnel::APP_API)
}

/// Attach sync, through the host's `sapphire-bridge`.
///
/// The framework connects when the server runs, and connects again whenever
/// the bridge restarts. A bridge that is not running is not fatal — the agent
/// serves its workspace without sync and picks the bridge up once it starts —
/// but any other failure (a bridge of another version, say) stops the start.
/// This is `AppServer::sync_via_bridge`, keeping the runtime for `placement`
/// to ask.
pub fn with_sync(server: AppServer) -> Result<(AppServer, Arc<SyncRuntime>)> {
    let runtime = Arc::new(SyncRuntime::connecting(
        &APP_CTX,
        BridgeConnector::standard(APP_CTX.app_name, VERSION),
        std::env::current_exe().context("locating this executable")?,
        ManagedBy::Service,
    ));
    Ok((server.sync(Arc::clone(&runtime)), runtime))
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
/// `restart`, and `serve` exits with [`RESTART_EXIT_CODE`] once the agent
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

/// Run the app server without the agent, for a config the agent cannot use.
///
/// A new host has no workspace yet, and the agent's config is partly in it
/// (`.sapphire-agent/config.toml`): the workspace verbs that would fetch it —
/// `workspace init --sync --remote-workspace` above all — need a running
/// server, so refusing to serve would leave nothing to fetch it with. The
/// server comes up as it would around the agent (IPC, the selected
/// workspace, sync through the bridge), and the agent with it once the
/// config can be used.
///
/// The host config and the served workspace's config are watched; a change
/// to either — written by hand, or arriving through sync — exits with
/// [`RESTART_EXIT_CODE`] so the service manager starts again and tries the
/// config again. A start that still fails comes back here.
pub async fn serve_without_agent(
    error: anyhow::Error,
    host_config: &Path,
    selected: Option<&Path>,
    edition: Edition,
) -> Result<()> {
    tracing::error!("{error:#}");
    tracing::warn!(
        "the agent is not running, only the {0} server: `{0} workspace` and the other \
         framework verbs work, and the agent starts once the config at {1} (or the \
         workspace's .{0}/config.toml) is fixed",
        APP_CTX.app_name,
        host_config.display()
    );

    let shutdown = CancellationToken::new();
    let (server, _sync) = with_sync(build(edition))?;
    let host = Arc::clone(server.host());
    let server_task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let result = server.run().await;
            shutdown.cancel();
            result
        })
    };

    // Until the server has restored the selection its root is unset; the
    // selection stands in so that is not mistaken for a switch.
    let selected = selected.map(Path::to_path_buf);
    let configs = || {
        let root = host.root().or_else(|| selected.clone());
        config_contents(host_config, root.as_deref())
    };
    let at_start = configs();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(SWITCH_POLL) => {}
        }
        if configs() != at_start {
            tracing::info!(
                "the config changed; exiting with {RESTART_EXIT_CODE} to start the agent again"
            );
            std::process::exit(RESTART_EXIT_CODE);
        }
    }
    server_task.await.context("the app server panicked")??;
    Ok(())
}

/// The host config and `root`'s workspace config, as they read now: what
/// [`serve_without_agent`] compares to notice a change. A file that cannot
/// be read is `None`, so one appearing is a change too.
fn config_contents(host_config: &Path, root: Option<&Path>) -> [Option<Vec<u8>>; 2] {
    [
        std::fs::read(host_config).ok(),
        root.and_then(|root| std::fs::read(crate::config::workspace_config_path(root)).ok()),
    ]
}

/// Which edition runs the server: the release file `update` installs is the
/// edition's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    /// `sapphire-agent-headless`, the command line alone.
    Headless,
    /// `sapphire-agent-desktop`, the desktop app.
    Desktop,
}

impl Edition {
    /// Where the edition's releases are published: what `update` installs
    /// from, and where the server looks once a day for a newer one. Both are
    /// in the shared `v{version}` release (release-binaries.yml), as
    /// `<crate>-<os>-<arch>`; only the headless edition is on crates.io.
    pub fn releases(self) -> ReleaseSource {
        let source = ReleaseSource::github("sapphire-turtle", "sapphire-agent");
        match self {
            Edition::Headless => source
                .asset("sapphire-agent-headless")
                .cargo_crate("sapphire-agent-headless"),
            Edition::Desktop => source.asset("sapphire-agent-desktop"),
        }
    }
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

    /// What brings a config-less server back to the agent: the host config
    /// appearing, and the workspace config arriving (through sync, say).
    #[test]
    fn a_config_appearing_reads_as_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().join("config.toml");
        let root = tmp.path().join("ws");
        let at_start = config_contents(&host, Some(&root));

        std::fs::write(&host, "[anthropic]\n").unwrap();
        let with_host = config_contents(&host, Some(&root));
        assert_ne!(with_host, at_start);

        std::fs::create_dir_all(root.join(".sapphire-agent")).unwrap();
        std::fs::write(root.join(".sapphire-agent/config.toml"), "x = 1\n").unwrap();
        assert_ne!(config_contents(&host, Some(&root)), with_host);
        assert_eq!(
            config_contents(&host, Some(&root)),
            config_contents(&host, Some(&root)),
            "unchanged files are no change"
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
