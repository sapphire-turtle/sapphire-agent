// matrix-sdk's E2EE futures nest deeply enough that proving `Send` on the
// Matrix channel's `listen` future exceeds the default type-checking recursion
// limit once the framework's redb/tantivy types are also in the graph.
#![recursion_limit = "256"]

mod acp_session;
mod agent;
mod agents;
mod ambient;
mod app_server;
mod autonomous;
mod autonomous_config;
mod avatar_store;
mod channel;
mod chat;
mod cli_init;
mod config;
mod config_layer;
mod context_compression;
mod device_auth;
mod frontmatter;
mod heartbeat;
mod heartbeat_config;
mod image_cache;
mod mcp_client;
mod memory_compaction;
mod periodic_log;
mod placement;
mod provider;
mod serve;
mod session;
mod session_log;
mod session_storage;
mod skills;
mod subagent_cache;
mod timer;
mod tool_payload_cache;
mod tools;
mod voice;
mod workspace;

use agent::Agent;
use anyhow::{Context, Result};
use channel::discord::DiscordChannel;
use channel::matrix::MatrixChannel;
use clap::{Parser, Subcommand};
use config::{Config, LoadedConfig};
use heartbeat::Heartbeat;
use periodic_log::{
    catchup_missing_daily_digests, catchup_pending_daily_logs, catchup_pending_monthly_logs,
    catchup_pending_weekly_logs, catchup_pending_yearly_logs,
};
use provider::registry::ProviderRegistry;
use sapphire_framework::workspace::AppContext;

static APP_CTX: AppContext = AppContext::new("sapphire-agent").allow_external_paths();

/// Inject host-platform paths into [`APP_CTX`] before any code touches a
/// [`SwWorkspace`]. The framework deliberately does not depend on `dirs`, so
/// each host app resolves its own cache / data directories at startup.
/// Make ring rustls's process-wide crypto provider.
///
/// rustls picks a default on its own only when exactly one backend is
/// compiled in, and matrix-sdk 0.19 adds aws-lc-rs beside the ring that
/// reqwest and the rest already use. reqwest names its provider and does not
/// care, but anything building a `ClientConfig::builder()` would panic with
/// both present. ring keeps every connection on the provider it used before.
fn install_crypto_provider() {
    // Err only when one is already installed, which is what we want anyway.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn init_app_ctx() {
    let base = directories::BaseDirs::new();
    let cache_dir = base
        .as_ref()
        .map(|b| b.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
        .join(env!("CARGO_PKG_NAME"));
    let data_dir = base
        .as_ref()
        .map(|b| b.data_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
        .join(env!("CARGO_PKG_NAME"));
    APP_CTX.set_cache_dir(cache_dir);
    APP_CTX.set_data_dir(data_dir);
    // Where the agent's own `config.toml` has always lived, so the
    // framework's `workspace.toml` sits beside it, and the framework's
    // `[http]` and `[update]` tables share the file itself.
    APP_CTX.set_config_dir(
        base.as_ref()
            .map(|b| b.config_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir)
            .join(APP_CTX.app_name),
    );
}
use session::SessionStore;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};
use workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "sapphire-agent",
    arg_required_else_help = true,
    about = "Personal AI assistant — Anthropic + Matrix/Discord",
    // Without this, `--help` shows the flattened `FrameworkCommand`'s doc
    // comment as the long description instead of `about`.
    long_about = None
)]
struct Cli {
    /// Path to config file (default: ~/.config/sapphire-agent/config.toml)
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Listen for HTTP on this address (e.g. 127.0.0.1:3170), overriding
    /// `[serve]`. Without either, `serve` opens no HTTP port.
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Seed a workspace with the files the agent reads.
    Init {
        /// Directory to seed. Created if missing; defaults to the current one.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Validate the config file and exit
    Verify,
    /// Speak ACP on stdio, for an editor to start as its agent: relayed to
    /// the agent running on this host, which runs the conversation. Needs
    /// `[acp].stdio_room_profile` in the host config.
    Acp,
    /// Chat with the agent running on this host: a REPL, or one message
    /// with `-m`. Needs `[cli].room_profile` in the host config.
    Chat {
        /// Grain-id of an existing session to resume (e.g. a3b7k9p).
        #[arg(long)]
        session: Option<String>,

        /// List the sessions and exit.
        #[arg(long)]
        list: bool,

        /// Send a single message and exit instead of entering the REPL.
        /// Useful as a CJK-safe fallback or for IDE/editor integration.
        #[arg(short, long, value_name = "TEXT")]
        message: Option<String>,

        /// Print the session's history and exit (no message sent, no REPL).
        #[arg(long)]
        history: bool,

        /// Emit machine-readable JSON for --list, --history and --message;
        /// ignored in the REPL.
        #[arg(long)]
        json: bool,
    },
    /// The framework's verbs: serve, status, service, update, workspace,
    /// workgroup, device, embedding, external-device, config.
    #[command(flatten)]
    Framework(sapphire_framework::server::FrameworkCommand),
}

/// Whether `meta`'s session counts toward `namespace`'s daily-log
/// catch-up. Mirrors `Heartbeat::namespace_for_session`
/// (`heartbeat.rs`): `/rpc`, `device-default`, and `acp` sessions carry
/// their own resolved namespace in metadata — trust that directly,
/// falling back to the default namespace for old files that predate
/// the field. Every other channel (Matrix/Discord `"channel"`
/// sessions) has no such field and resolves via
/// `Config::namespace_for_room` instead.
///
/// Extracted to a named function (rather than inlined as a closure)
/// so it can be unit-tested directly.
fn daily_log_catchup_predicate(
    meta: &session::SessionMeta,
    config: &config::Config,
    namespace: &str,
) -> bool {
    if meta.channel == "rpc" || meta.channel == "device-default" || meta.channel == "acp" {
        meta.namespace
            .as_deref()
            .unwrap_or(config::DEFAULT_NAMESPACE_NAME)
            == namespace
    } else {
        config.namespace_for_room(&meta.room_id) == namespace
    }
}

/// One-word tag naming where a setting's effective value came from, for `verify`.
fn layer_tag(
    provenance: &std::collections::BTreeMap<String, config_layer::Layer>,
    path: &str,
) -> &'static str {
    match provenance.get(path) {
        Some(config_layer::Layer::Workspace) => "workspace",
        Some(config_layer::Layer::Host) => "host",
        None => "default",
    }
}

/// Group unrecognised config keys by the file they came from, for one
/// warning per file rather than one per key.
///
/// The file is named through `provenance` so the message points at the
/// file the person has to edit; a key that cannot be attributed to either
/// layer is grouped under a neutral label rather than blamed on the host
/// config, which would send them to the wrong file.
///
/// Returns pairs rather than logging directly so `verify` can print the
/// same grouping it warns with, and so a test can read it.
fn group_unknown_keys<'a>(
    unknown: &'a [String],
    provenance: &std::collections::BTreeMap<String, config_layer::Layer>,
    host_path: &std::path::Path,
    workspace_path: Option<&std::path::Path>,
) -> Vec<(String, Vec<&'a str>)> {
    let mut by_file: std::collections::BTreeMap<String, Vec<&'a str>> = Default::default();
    for key in unknown {
        let file = match config_layer::layer_of(key, provenance) {
            Some(config_layer::Layer::Workspace) => workspace_path.map(|p| p.display().to_string()),
            Some(config_layer::Layer::Host) => Some(host_path.display().to_string()),
            None => None,
        }
        .unwrap_or_else(|| "the configuration".to_string());
        by_file.entry(file).or_default().push(key.as_str());
    }
    by_file.into_iter().collect()
}

/// `verify`'s device section: the bindings in config, and — when the bridge
/// is running — the external devices allowed to use this agent, so a device
/// with no room profile, or a selector naming no device, shows up before a
/// request is refused for it.
async fn verify_external_devices(auth: &device_auth::DeviceAuth) {
    use sapphire_framework_bridge_api::{ExternalDeviceOutcome, ExternalDeviceRequest};

    println!("  Device bindings   :");
    for (selector, rp) in auth.bindings() {
        println!("    {selector:<20} -> room_profile '{rp}'");
    }
    let listed = match sapphire_framework_bridge_client::BridgeClient::connect_running(
        "cli",
        env!("CARGO_PKG_VERSION"),
    )
    .await
    {
        Ok(client) => client
            .external_device_request(ExternalDeviceRequest::List)
            .await
            .map_err(anyhow::Error::from),
        Err(e) => Err(e.into()),
    };
    let devices = match listed {
        Ok(ExternalDeviceOutcome::List(devices)) => devices,
        Ok(_) => return,
        Err(e) => {
            println!("  External devices  : not checked, the sapphire bridge is unreachable ({e})");
            return;
        }
    };
    let live: Vec<_> = devices
        .iter()
        .filter(|d| d.retired_at.is_none() && d.apps.iter().any(|a| a == device_auth::APP_NAME))
        .collect();
    println!(
        "  External devices  : {} allowed to use {}",
        live.len(),
        device_auth::APP_NAME
    );
    for d in &live {
        match auth.room_profile_for_device(d.id, &d.name) {
            Some(rp) => println!("    {:<20} {} -> room_profile '{rp}'", d.name, d.id),
            None => println!(
                "    {:<20} {} -> UNBOUND: its token is refused",
                d.name, d.id
            ),
        }
    }
    for (selector, _) in auth.bindings() {
        let known = devices.iter().any(|d| {
            d.name == selector
                || selector
                    .parse::<grain_id::GrainId>()
                    .is_ok_and(|id| id == d.id)
        });
        if !known {
            println!("    {selector:<20} names no external device");
        }
    }
}

/// Load the layered config, refusing one that uses removed settings.
fn load_config(
    config_path: &std::path::Path,
    workspace_root: Option<&std::path::Path>,
) -> Result<LoadedConfig> {
    let loaded = Config::load_layered_in(config_path, workspace_root)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;
    if loaded.config.standby_mode == Some(true) {
        anyhow::bail!(
            "config at {} sets `standby_mode = true`, which was removed in the \
             sapphire-framework `main` migration: the framework no longer ships \
             local-workspace auto-sync, so a standby node has nothing left to \
             sync and would instead come up as a second fully-active agent \
             (duplicate Matrix/Discord replies, two processes racing on \
             MEMORY.md compaction and daily-log generation in the same \
             workspace). Delete the `standby_mode` line from this config to \
             upgrade.",
            config_path.display()
        );
    }

    let migration_errors = loaded.config.migration_errors();
    if !migration_errors.is_empty() {
        anyhow::bail!(
            "config at {} uses settings that were removed:\n\n  - {}\n",
            config_path.display(),
            migration_errors.join("\n\n  - ")
        );
    }
    Ok(loaded)
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // `tracing_subscriber::fmt` defaults to stdout. Every command's
        // output is `println!` (`external-device add` prints the minted
        // token there), so sending logs to stderr keeps a stray log line
        // out of anything piped from a command's output.
        .with_writer(std::io::stderr)
        .init();

    install_crypto_provider();
    init_app_ctx();

    let cli = Cli::parse();

    // Before the config load below: `init` is what an operator runs when there
    // is no host config yet, so requiring one to reach it would make the
    // command unusable exactly when it is needed.
    if let Command::Init { path } = &cli.command {
        return cli_init::run(path.clone());
    }
    // A relay to the running agent, which holds the config: nothing to load
    // here, and stdout belongs to ACP.
    if let Command::Acp = &cli.command {
        return serve::acp_tunnel::run_stdio().await;
    }
    // Likewise a client of the running agent.
    if let Command::Chat {
        session,
        list,
        message,
        history,
        json,
    } = &cli.command
    {
        return chat::run(chat::ChatOptions {
            session: session.clone(),
            list: *list,
            message: message.clone(),
            history: *history,
            json: *json,
        })
        .await;
    }

    // The framework's verbs other than `serve` talk to the running server
    // (which relays the bridge's), and need no agent config either.
    let cli = match cli.command {
        Command::Framework(command)
            if !matches!(command, sapphire_framework::server::FrameworkCommand::Serve) =>
        {
            // The agent has no keys of its own in the framework's settings
            // registry (`()`): its `config.toml` tables are its own to read.
            let code = command
                .dispatch(
                    &APP_CTX,
                    (),
                    |_: &sapphire_framework::server::Settings| app_server::build(),
                    app_server::VERSION,
                )
                .await?;
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        command => Cli { command, ..cli },
    };

    let config_path = cli.config.unwrap_or_else(Config::default_path);
    // The framework's selected workspace decides which workspace the agent
    // serves; `workspace_dir` is only where a first start migrates it from.
    let selection = app_server::selection()?;
    let serving = matches!(
        cli.command,
        Command::Framework(sapphire_framework::server::FrameworkCommand::Serve)
    );
    let loaded = load_config(&config_path, selection.as_ref().map(|s| s.root.as_path()))
        // `serve` refuses to start on what only the registry can see, so a
        // config it would refuse is one it serves without below.
        .and_then(|loaded| {
            if serving {
                ProviderRegistry::from_config(&loaded.config)
                    .context("Failed to build provider registry")?;
            }
            Ok(loaded)
        });
    let LoadedConfig {
        config,
        workspace_dir,
        rejected,
        unknown,
        provenance,
        workspace_path,
    } = match loaded {
        Ok(loaded) => loaded,
        // Without a usable config there is no agent to run, but the app
        // server still has to be up: `workspace init --remote-workspace`
        // and the other workspace verbs are how a new host gets the
        // workspace its config lives in.
        Err(e) if serving => {
            let selected = selection.as_ref().map(|s| s.root.as_path());
            return app_server::serve_without_agent(e, &config_path, selected).await;
        }
        Err(e) => return Err(e),
    };
    if let (Some(selected), Some(dir)) = (&selection, &config.workspace_dir) {
        let configured = config::resolve_workspace_dir(Some(dir), &config_path);
        if configured.canonicalize().ok().as_deref() != Some(selected.root.as_path()) {
            tracing::warn!(
                "`workspace_dir` ({}) is ignored: the selected workspace is {}. Change it with \
                 `sapphire-agent workspace select <dir>`, and remove `workspace_dir` from {}.",
                configured.display(),
                selected.root.display(),
                config_path.display()
            );
        }
    }

    if !rejected.is_empty() {
        tracing::warn!(
            "Ignoring {} key(s) in the workspace config at {} that the workspace layer may not \
             set: {}. API keys and other credentials, `workspace_dir`, `[tools.host_access]` \
             and `[serve]` are host-local by design; set them in {} instead.",
            rejected.len(),
            workspace_path
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            rejected.join(", "),
            config_path.display()
        );
    }

    for (file, keys) in group_unknown_keys(
        &unknown,
        &provenance,
        &config_path,
        workspace_path.as_deref(),
    ) {
        tracing::warn!(
            "Ignoring {} unrecognised key(s) in {file}: {}. Nothing reads these, so the \
             agent is running as if they were absent — check for a typo, or for a setting \
             written under the wrong table.",
            keys.len(),
            keys.join(", ")
        );
    }

    if config.timer.retired_presets.is_some() {
        tracing::warn!(
            "Ignoring [[timer.preset]] in the config: timer presets moved to sapphire-timer, \
             which the agent reaches through `[timer] server`. Recreate them there (its \
             `preset_add` tool) and remove them from this file."
        );
    }

    match cli.command {
        // Returned above, before the config was loaded.
        Command::Init { .. } => unreachable!("`init` is dispatched ahead of the config load"),
        Command::Acp => unreachable!("`acp` is dispatched ahead of the config load"),
        Command::Chat { .. } => {
            unreachable!("`chat` is dispatched ahead of the config load")
        }
        Command::Framework(sapphire_framework::server::FrameworkCommand::Serve) => {
            let bind = cli.bind;
            let workspace_dir = match &selection {
                Some(selected) => selected.root.clone(),
                None => {
                    let root = app_server::adopt(&workspace_dir)?;
                    tracing::info!(
                        "selected {} as this agent's workspace (from `workspace_dir`); change it \
                         with `sapphire-agent workspace select <dir>`",
                        root.display()
                    );
                    root
                }
            };

            // ── Migrate pre-namespace memory layout (one-time) ─────────────
            if let Err(e) = migrate_pre_namespace_layout(&workspace_dir) {
                anyhow::bail!("Memory layout migration failed: {e:#}");
            }
            // ── Consolidate sessions/{matrix,discord} into sessions/channel ──
            // Both chat channels can run concurrently now and share one
            // session store keyed by ULID; the originating channel is
            // still recorded inside each session's metadata.
            let sessions_base_for_migration = config.resolved_sessions_dir(&workspace_dir);
            if let Err(e) = migrate_per_channel_sessions(&sessions_base_for_migration) {
                anyhow::bail!("Session layout migration failed: {e:#}");
            }
            // ── Move sessions/<kind>/* into sessions/<namespace>/<kind>/* ──
            // Reads each session's meta.namespace (falling back to "default")
            // and slots the file under the matching namespace directory so
            // sapphire-retrieve can scope indexing by directory rather than
            // by reading each file's frontmatter.
            if let Err(e) = migrate_sessions_to_namespaced_layout(&sessions_base_for_migration) {
                anyhow::bail!("Session namespace migration failed: {e:#}");
            }
            // ── Split sessions/<ns>/{api,rpc}/ into cross-device + legacy-voice ─
            // Quarantines pre-redesign voice files (their `voice-<hash>`
            // names can't be re-hydrated into the new
            // `(device_id, room_profile)` keying) and moves remaining
            // sessions into the new `cross-device/` directory, rewriting
            // stored `meta.channel = "api"` → `"rpc"` along the way.
            if let Err(e) = migrate_to_device_default_layout(&sessions_base_for_migration) {
                anyhow::bail!("Device-default session migration failed: {e:#}");
            }

            // ── Bootstrap file loader (AGENTS.md, SOUL.md, MEMORY.md …) ────
            let workspace = Arc::new(Workspace::new(workspace_dir.clone(), config.digest.clone()));

            // ── App server: IPC, the current workspace, signals ─────────────
            // The framework's server owns the workspace (and its redb cache,
            // which only one process may open), restores the selection as it
            // starts, and handles SIGTERM / SIGINT. Everything below is built
            // around the workspace it opens. `shutdown` is the one signal the
            // agent's own tasks stop on: the server stopping, or a switch to
            // another workspace (which is a restart — see `app_server`).
            let shutdown = CancellationToken::new();
            let restart = Arc::new(AtomicBool::new(false));
            // `sapphire-agent acp` and `sapphire-agent chat` reach the agent
            // through the server's IPC endpoint; the agent they reach is
            // attached once built.
            let acp_tunnel = serve::acp_tunnel::AcpTunnel::default();
            let chat_tunnel = serve::chat_ipc::ChatTunnel::default();
            let server = app_server::build().extend({
                let acp = acp_tunnel.clone();
                let chat = chat_tunnel.clone();
                move |router| chat.router(acp.router(router))
            });
            let (server, sync_runtime) = app_server::with_sync(server)?;
            let host = Arc::clone(server.host());
            let server_task = {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let result = server.run().await;
                    shutdown.cancel();
                    result
                })
            };
            let (served_root, ws_state) = tokio::select! {
                opened = app_server::open_workspace(&host) => opened?,
                () = shutdown.cancelled() => {
                    // The server stopped before a workspace opened: a signal,
                    // or a failed start (another instance holds the endpoint).
                    server_task.await.context("the app server panicked")??;
                    return Ok(());
                }
            };
            if served_root != workspace_dir {
                // Selected in the moment between the config load and the
                // server's start: the config layer read was another
                // workspace's. Start again around the one being served.
                tracing::warn!(
                    "the server serves {} but the agent was configured for {}; restarting",
                    served_root.display(),
                    workspace_dir.display()
                );
                std::process::exit(app_server::RESTART_EXIT_CODE);
            }
            let switch_watch = app_server::watch_for_switch(
                Arc::clone(&host),
                served_root.clone(),
                shutdown.clone(),
                Arc::clone(&restart),
            );
            // ── Placement: which host runs what, once synced ───────────────
            let placement = placement::Placement::synced(sync_runtime, served_root);
            placement::spawn_claim_keeper(
                Arc::clone(&placement),
                workspace_dir.clone(),
                placement::local_channels(&config),
                shutdown.clone(),
            );
            // `sync_interval_minutes` is the re-index cadence. The framework
            // removed local-workspace auto-sync, so a tick is an mtime-based
            // refresh of the retrieve cache and nothing else.
            let ws_sync_interval = config
                .sync_interval_minutes
                .filter(|&m| m > 0)
                .map(|m| std::time::Duration::from_secs(m as u64 * 60));
            if let Err(e) = ws_state.sync_retrieve() {
                tracing::warn!("Initial workspace re-index failed: {e}");
            }

            // ── Tools ───────────────────────────────────────────────────────
            let tool_set = tools::default_tool_set(
                Arc::clone(&ws_state),
                config.tools.tavily_api_key.clone(),
                &config.tools.mcp_servers,
                config.day_boundary_hour,
            )
            .await;

            // ── Timer notifications (sapphire-timer over MCP) ───────────────
            // The timer runs in the MCP server `[timer] server` names; the
            // LLM drives it through that server's `mcp__<server>__timer_*`
            // tools. The bridge records where each run was started from and
            // announces its step / finish events there. The agent and
            // serve_state refs are installed into the dispatcher below, once
            // those exist (Weak both ways).
            let timer_dispatch = timer::ChannelDispatch::new();
            if let Some(server) = &config.timer.server {
                match tool_set.mcp_client(server) {
                    Some(client) => {
                        timer::TimerBridge::new(Arc::clone(&timer_dispatch) as _).spawn(client)
                    }
                    None if config.tools.mcp_servers.iter().any(|s| &s.name == server) => {
                        tracing::warn!(
                            "[timer] server '{server}' did not connect at start-up; timer \
                             notifications are off until the agent restarts"
                        )
                    }
                    None => tracing::warn!(
                        "[timer] server '{server}' names no entry in [[tools.mcp_servers]]; \
                         timer notifications are off"
                    ),
                }
            }

            // ── Subagents ─────────────────────────────────────────────────
            // Loaded from `<workspace>/agents/*.md` — same shape as
            // `<workspace>/heartbeat/*.md`. Registered outside
            // `default_tool_set` (rather than inside it) so that
            // function's existing call sites, including the test in
            // `src/agent.rs`, stay untouched by this feature. Registered
            // when at least one definition loaded — a tool with nobody to
            // delegate to has no reason to be offered — and also when
            // `[tools.admin].room_profiles` names a profile: `agent_config` can
            // create a definition while the process is running.
            let agent_defs = agents::load_agents_dir(&workspace_dir.join("agents"));
            // Taken before `new` moves the list: an empty list is not an
            // error, it is the state `agent_config` exists to change.
            let no_agent_defs = agent_defs.is_empty();
            let profile_errors = config.validate_subagent_profiles(&agent_defs);
            if !profile_errors.is_empty() {
                anyhow::bail!(
                    "invalid subagent profile references:\n  - {}",
                    profile_errors.join("\n  - ")
                );
            }
            // Registered when there is something to delegate to, and also
            // when `[tools.admin].room_profiles` names a profile: `agent_config`
            // can create a definition while the process is running. `Arc` rather
            // than a bare `Box` because `agent_config` holds a `Weak` to it —
            // same shape `SkillTool` uses.
            let subagent = Arc::new(tools::subagent::SubagentTool::new(agent_defs));
            if !no_agent_defs || config.config_tools_enabled() {
                tool_set
                    .register_tool(Box::new(Arc::clone(&subagent)))
                    .await;
            }

            // ── Skills ────────────────────────────────────────────────────
            // Registered unconditionally: unlike subagents there is
            // nothing to load at startup, because the directory lives
            // on the editor's machine and is resolved per session.
            // `visible_tool_predicate` plus the namespace switch decide
            // whether it is ever offered.
            //
            // `skill_install`/`skill_update`/`skill_uninstall` share the
            // same `SkillTool` the read-only `skill` uses (via `Arc`,
            // wrapped in a `Box<dyn Tool>` each `ToolSet` slot owns
            // independently) so a successful change can invalidate the
            // one cache all four read from — see `SkillTool::cache`'s
            // doc.
            let skill_tool = Arc::new(tools::skill_tools::SkillTool::new());
            tool_set
                .register_tool(Box::new(Arc::clone(&skill_tool)))
                .await;
            tool_set
                .register_tool(Box::new(tools::skill_tools::SkillInstallTool::new(
                    Arc::clone(&skill_tool),
                )))
                .await;
            tool_set
                .register_tool(Box::new(tools::skill_tools::SkillUpdateTool::new(
                    Arc::clone(&skill_tool),
                )))
                .await;
            tool_set
                .register_tool(Box::new(tools::skill_tools::SkillUninstallTool::new(
                    skill_tool,
                )))
                .await;

            // ── Session store base directory ────────────────────────────────
            let sessions_base = config.resolved_sessions_dir(&workspace_dir);

            // ── Tool-payload cache (workspace-external, content-addressed) ──
            // Every session store below — cross-device, device-default,
            // mcp, channel — and the ACP store further down keep tool
            // both halves of a tool call out of the JSONL itself,
            // content-addressed by hash,
            // for the same reason images are. Built here, before any of
            // them, so the same `Option<Arc<...>>` can be handed to all
            // four SessionStore kinds plus AcpSessionStore.
            //
            // Resolves once at startup, degrading to `None` on a missing
            // platform cache dir or an open failure (e.g. read-only
            // `~/.cache` / `%LOCALAPPDATA%`) rather than aborting startup
            // — mirrors `image_cache` below. A daemon whose whole design
            // premise is that losing a cache is survivable must not make
            // its boot path the one place where it isn't: this cache
            // being unavailable would otherwise stop `/rpc`, Matrix,
            // Discord and voice too, in every deployment, even ones that
            // never use ACP.
            //
            // Every session store persists tool traffic now, so this
            // cache is on the request path of every transport — not just
            // ACP. `None` still degrades rather than aborting startup: a
            // payload written without it keeps its call and its pairing
            // and reads back as a stand-in, which is a thinner session
            // rather than an unloadable one.
            let tool_payload_cache: Option<Arc<tool_payload_cache::ToolPayloadCache>> =
                match tool_payload_cache::ToolPayloadCache::default_dir() {
                    Some(dir) => match tool_payload_cache::ToolPayloadCache::open(dir.clone()) {
                        Ok(cache) => {
                            tracing::info!("Tool-payload cache opened at {dir:?}");
                            Some(cache)
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Tool-payload cache open failed ({e:?}); tool payloads will not be cached"
                            );
                            None
                        }
                    },
                    None => {
                        tracing::warn!(
                            "No platform cache dir resolvable; tool-payload cache disabled"
                        );
                        None
                    }
                };

            // ── Provider registry ────────────────────────────────────────────
            // Builds the Anthropic provider plus any `[providers.<name>]`
            // entries, then validates every profile/room reference. Failure
            // here is fatal — better to refuse to start than to surprise the
            // user mid-session with a misrouted request.
            let registry = Arc::new(
                ProviderRegistry::from_config(&config)
                    .context("Failed to build provider registry")?,
            );

            // ── Cross-device session store (sessions/<namespace>/cross-device/) ─
            // Cross-device sessions are the user-selectable, multi-device
            // conversation threads (resumed via `--resume <grain-id>`).
            // The bundled session migration moves pre-PR-3 files into
            // this dir from the legacy `api/` and `rpc/` locations.
            let cross_device_session_store = Arc::new(SessionStore::with_workspace(
                sessions_base.clone(),
                "cross-device",
                Arc::clone(&ws_state),
                tool_payload_cache.clone(),
            ));

            // ── Device-default session store (sessions/<ns>/device-default/) ─
            // Lazy-created, daily-rotated per `(device_id, room_profile)` —
            // the heartbeat target and the implicit session for a satellite
            // with nothing else selected. See #122.
            let device_default_session_store = Arc::new(SessionStore::with_workspace(
                sessions_base.clone(),
                "device-default",
                Arc::clone(&ws_state),
                tool_payload_cache.clone(),
            ));

            // ── Channel session store (sessions/<namespace>/channel/) ──────
            // Sessions from every chat channel land here; each one still
            // records its originating channel name in metadata. Built
            // unconditionally — construction touches no filesystem — so
            // the `session_list` / `session_read` tools can read chat
            // sessions written by an earlier run even when this process
            // has no channel configured.
            let channel_session_store = Arc::new(SessionStore::with_workspace(
                sessions_base.clone(),
                "channel",
                Arc::clone(&ws_state),
                tool_payload_cache.clone(),
            ));

            // ── Autonomous session store (sessions/<ns>/autonomous/) ────────
            // Where the idle loop's sessions live. Built unconditionally —
            // construction touches nothing — so `session_list` can read what
            // last night's loop wrote even in a process that has autonomous
            // sessions turned off. Directories are named `{agent-day}-{uuid}`
            // so an overnight run reads as a run in a directory listing; the
            // session's own metadata is in its `session.md`, unchanged.
            let autonomous_session_store = Arc::new(
                SessionStore::with_workspace(
                    sessions_base.clone(),
                    "autonomous",
                    Arc::clone(&ws_state),
                    tool_payload_cache.clone(),
                )
                .with_dated_files(config.day_boundary_hour),
            );

            // ── Voice providers + ServeState (built early) ──────────────────
            // ServeState owns the voice_subscribers registry that heartbeat
            // pushes through, so it has to exist before heartbeat is spawned.
            // The STT/TTS bundle download still happens once, on the
            // blocking pool, before any channel/heartbeat task starts —
            // a small startup-latency cost in exchange for a heartbeat
            // path that can target voice satellites.
            let voice_providers = if config.stt_providers.is_empty()
                && config.tts_providers.is_empty()
            {
                None
            } else {
                let cfg = config.clone();
                let providers =
                    tokio::task::spawn_blocking(move || voice::VoiceProviders::from_config(&cfg))
                        .await
                        .map_err(|e| anyhow::anyhow!("voice provider init panicked: {e}"))??;
                Some(Arc::new(providers))
            };

            // ── Device auth (bearer token -> device -> room profile) ────────
            // Built once, here, and shared as one `Arc` between ambient
            // ingest and `ServeState` (`/rpc`, `/a2a`, `/acp`) so
            // there is exactly one answer to "who is this token" in the
            // process. Tokens are checked by the sapphire bridge; one that
            // is not running is not fatal here (it may come up later), but
            // every authenticated request answers 503 until it does.
            let device_auth = Arc::new(device_auth::DeviceAuth::bridge(&config.room_profiles)?);
            if let Err(e) = sapphire_framework_bridge_client::BridgeClient::connect_running(
                "agent",
                env!("CARGO_PKG_VERSION"),
            )
            .await
            {
                tracing::warn!(
                    "device auth: cannot reach the sapphire bridge ({e}); every authenticated \
                     endpoint answers 503 until it is running"
                );
            }

            // ── Ambient audio ingest (optional) ─────────────────────────────
            // Built here — after `voice_providers` exists, before it is
            // moved into `ServeState::new` below — so it can resolve
            // `[ambient].stt_provider` against the same registry. A borrow
            // is enough; `build` never needs to own the registry. Shares
            // `device_auth` with `ServeState` rather than resolving its own.
            // Every failure here is fatal and loud: an ambient subsystem
            // that starts but cannot authenticate, transcribe, or store
            // looks exactly like a broken device from the outside, and the
            // device has no way to tell you.
            let ambient_runtime = ambient::startup::build(
                &config,
                &workspace_dir,
                voice_providers.as_deref(),
                Arc::clone(&device_auth),
            )
            .context("ambient audio ingest failed to start")?;

            // ── Image cache (workspace-external) ──────────────────────────
            // Resolves once at startup. A missing platform cache dir
            // (rare; effectively only headless edge cases) or a config
            // explicitly disabling the cache yields `None`, which makes
            // the scrub / hydrate helpers no-op and persistence falls
            // back to the SHA-256 text marker in SessionStore::append.
            let image_cache: Option<Arc<image_cache::ImageCache>> = if config.image_cache.enabled {
                let resolved = config
                    .image_cache
                    .dir
                    .clone()
                    .or_else(image_cache::ImageCache::default_dir);
                match resolved {
                    Some(dir) => match image_cache::ImageCache::open(dir.clone()) {
                        Ok(cache) => {
                            tracing::info!("Image cache opened at {dir:?}");
                            Some(cache)
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Image cache open failed ({e:?}); falling back to text-marker only"
                            );
                            None
                        }
                    },
                    None => {
                        tracing::warn!(
                            "No platform cache dir resolvable; image cache disabled (set [image_cache] dir = \"...\" to override)"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Register `recall_image` once the image cache is resolved.
            // Without a cache there's nothing to recall, so skip the
            // tool entirely — the model never sees it advertised.
            if let Some(cache) = image_cache.clone() {
                tool_set
                    .register_tool(Box::new(tools::builtin_tools::RecallImageTool::new(cache)))
                    .await;
            }

            // Register the ambient tools (`transcript_read`,
            // `speaker_candidates`, `speaker_promote`) only once the
            // subsystem actually started — same rule as `recall_image`
            // above: the model never sees a tool that cannot work.
            // `spawn` is what starts the subsystem's only background
            // tasks (the worker loop and the daily audio sweep), and
            // hands back the ingest router for `serve::run` to mount.
            let mut ambient_routes: Option<axum::Router> = None;
            if let Some(runtime) = ambient_runtime {
                let (routes, tools) = ambient::startup::spawn(runtime);
                for tool in tools {
                    tool_set.register_tool(tool).await;
                }
                ambient_routes = Some(routes);
            }

            // The avatar tools are always registered and never offered by
            // default: `run_llm_turn` adds them only to a turn whose client
            // declared an avatar (see `tools::avatar_tools`).
            tool_set
                .register_tool(Box::new(tools::avatar_tools::SetExpressionTool))
                .await;
            tool_set
                .register_tool(Box::new(tools::avatar_tools::PlayActionTool))
                .await;

            // ── ACP session store (sessions/<namespace>/acp/) ───────────────
            // Separate from `cross_device_session_store`: ACP is an
            // externally-defined standard that will drift from the
            // format we chose for ourselves, and an editor's thread
            // list should not be showing Matrix conversations. Tool
            // results are kept in a workspace-external, content-addressed
            // cache rather than inline, for the same reason images are.
            let acp_session_store = Arc::new(acp_session::AcpSessionStore::new(
                sessions_base.clone(),
                tool_payload_cache.clone(),
            ));

            // ── Subagent child-conversation cache (workspace-external) ──────
            // Same directory family and degrade-not-abort treatment as the
            // tool-payload cache above. `None` costs the ability to resume a
            // subagent across calls — the `subagent` tool falls back to
            // one-shot and tells the model so — not startup.
            let subagent_cache: Option<Arc<subagent_cache::SubagentCache>> =
                match subagent_cache::SubagentCache::default_dir() {
                    Some(dir) => match subagent_cache::SubagentCache::open(
                        dir.clone(),
                        config.subagent_cache.max_history_bytes,
                    ) {
                        Ok(cache) => {
                            tracing::info!("Subagent cache opened at {dir:?}");
                            Some(cache)
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Subagent cache open failed ({e:?}); subagents will not be resumable"
                            );
                            None
                        }
                    },
                    None => {
                        tracing::warn!("No platform cache dir resolvable; subagent cache disabled");
                        None
                    }
                };

            let serve_state = Arc::new(serve::ServeState::new(
                config.clone(),
                Arc::clone(&registry),
                Arc::clone(&workspace),
                Arc::clone(&tool_set),
                Arc::clone(&cross_device_session_store),
                Arc::clone(&device_default_session_store),
                Arc::clone(&autonomous_session_store),
                voice_providers,
                image_cache.clone(),
                Arc::clone(&device_auth),
                acp_session_store,
                subagent_cache,
            ));
            // Wire serve_state into the timer dispatcher so voice-origin
            // timers can push their messages back to the satellite.
            timer_dispatch.set_serve_state(Arc::downgrade(&serve_state));
            acp_tunnel.attach(Arc::clone(&serve_state));
            chat_tunnel.attach(Arc::clone(&serve_state));

            // ── Autonomous sessions (off unless configured) ─────────────────
            // Spawned outside the channel block on purpose: this loop needs
            // `ServeState` and nothing else, so an ACP-only or voice-only
            // deployment gets the feature too. `enabled = false` (the
            // default) means no loop, no status file, no directory.
            if config.autonomous.enabled {
                autonomous::AutonomousLoop::new(
                    Arc::clone(&serve_state),
                    Some(Arc::clone(&channel_session_store)),
                )
                .with_placement(Arc::clone(&placement))
                .spawn();
            } else {
                tracing::info!("Autonomous sessions disabled by config");
            }

            // ── Session tools ───────────────────────────────────────────────
            // Registered here rather than in `default_tool_set` because
            // they read the session stores, which are built after it.
            // They replace the "today's cross-session notes" block that
            // used to be summarised into every system prompt: the model
            // asks when it wants to know, and the prompt prefix stops
            // changing underneath the provider's cache.
            {
                let sources = Arc::new(tools::session_tools::SessionSources::new(
                    config.clone(),
                    Some(Arc::clone(&channel_session_store)),
                    Arc::clone(&cross_device_session_store),
                    Arc::clone(&device_default_session_store),
                    Arc::clone(&autonomous_session_store),
                    Arc::clone(&serve_state.acp_session_store),
                ));
                tool_set
                    .register_tool(Box::new(tools::session_tools::SessionListTool::new(
                        Arc::clone(&sources),
                    )))
                    .await;
                tool_set
                    .register_tool(Box::new(tools::session_tools::SessionReadTool::new(
                        sources,
                    )))
                    .await;
            }

            // ── Config tools ────────────────────────────────────────────────
            // The agent's own heartbeat / autonomous / subagent definitions,
            // editable from a chat room an operator names in
            // `[tools.admin].room_profiles`. Registered here rather than in
            // `default_tool_set` because `task_test` needs `serve_state`,
            // which is built above, and because a `room_profiles` that names
            // nobody must register none of the four at all — see
            // `register_admin_tools`.
            tools::config_tools::register_admin_tools(
                &tool_set,
                &workspace_dir,
                config.clone(),
                Arc::clone(&ws_state),
                Some(&subagent),
                Arc::clone(&serve_state),
            )
            .await;

            // Captured below so main can await the agent's graceful
            // shutdown before returning. Without this, the tokio runtime
            // drops the spawned task the moment serve::run returns,
            // cancelling any in-flight LLM call (#48).
            let mut agent_handle: Option<tokio::task::JoinHandle<()>> = None;

            // ── Channel + Agent (Matrix and/or Discord, if configured) ──────
            let mut channel_agent: Option<Arc<Agent>> = None;
            if config.matrix.is_some() || config.discord.is_some() {
                let mut channel_list: Vec<(String, Arc<dyn channel::Channel>)> = Vec::new();
                if let Some(m) = &config.matrix {
                    channel_list.push(("matrix".to_string(), Arc::new(MatrixChannel::new(m))));
                }
                if let Some(d) = &config.discord {
                    channel_list.push((
                        "discord".to_string(),
                        Arc::new(
                            DiscordChannel::new(d)
                                .context("Failed to initialise Discord channel")?,
                        ),
                    ));
                }
                let channels = Arc::new(channel::Channels::new(
                    channel_list,
                    channel::seed_routing_from_config(&config),
                ));
                tracing::info!("Channels active: {}", channels.names().join(", "));

                // ── Agent ───────────────────────────────────────────────────
                let agent = Arc::new(Agent::new(
                    config.clone(),
                    Arc::clone(&channels),
                    Arc::clone(&registry),
                    Arc::clone(&workspace),
                    Some(Arc::clone(&tool_set)),
                    Arc::clone(&channel_session_store),
                    image_cache.clone(),
                ));
                // Wire the agent into the timer dispatcher so chat-origin
                // timers are announced through `Agent::trigger`.
                timer_dispatch.set_agent(Arc::downgrade(&agent));

                // ── refresh_system_prompt ───────────────────────────────
                // Registered here, right after the agent is built, and
                // only in this channel-configured branch: an ACP-only
                // configuration has no Agent and thus no snapshots, so
                // the pinned file reads alone are enough there. Held as
                // `Weak` to avoid a cycle — the tool set is itself owned
                // (transitively) by the agent.
                tool_set
                    .register_tool(Box::new(
                        tools::builtin_tools::RefreshSystemPromptTool::new(Arc::downgrade(&agent)),
                    ))
                    .await;

                // ── Periodic workspace re-index ─────────────────────────
                // `sync()` picks up session JSONLs and notes written
                // outside the agent. It used to also rebuild a
                // per-namespace "today's digest" block for the system
                // prompt; that block is gone, and `session_list` /
                // `session_read` answer the same question on demand.
                if let Some(dur) = ws_sync_interval {
                    tracing::info!(
                        "Periodic workspace re-index enabled: every {}s",
                        dur.as_secs()
                    );
                    let ws = Arc::clone(&ws_state);
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(dur);
                        tick.tick().await; // skip immediate fire
                        loop {
                            tick.tick().await;
                            {
                                let state = &ws;
                                match state.sync_retrieve() {
                                    Ok((u, r)) => tracing::info!(
                                        "Periodic ws sync: {u} upserted, {r} removed"
                                    ),
                                    Err(e) => tracing::warn!("Periodic ws sync failed: {e:#}"),
                                }
                            }
                        }
                    });
                }

                channel_agent = Some(Arc::clone(&agent));

                let agent_run = Arc::clone(&agent);
                let agent_shutdown = shutdown.clone();
                agent_handle = Some(tokio::spawn(async move {
                    if let Err(e) = agent_run.run(agent_shutdown).await {
                        tracing::error!("Agent error: {e:#}");
                    }
                }));
            }

            // ── Catch-up and heartbeat ──────────────────────────────────────
            // Outside the channel block: the logs and the compaction run on
            // the primary device of a synced workspace (`placement`), which
            // need not have a chat channel. The startup catch-up runs only if
            // this host already knows it is that device; the hourly
            // catch-up loop picks up what this skips.
            if placement.runs_background().await {
                // ── Catch-up: iterate every configured memory namespace ────
                // Each namespace's background tasks run on its own provider
                // (resolved via `background_provider_for_namespace`), so an
                // NSFW namespace can route to its local model up front
                // instead of bouncing through Anthropic refusal-fallback.
                let today_local = session::local_date_for_timestamp(
                    chrono::Local::now(),
                    config.day_boundary_hour,
                );
                for ns in config.all_memory_namespaces() {
                    let provider = registry.background_provider_for_namespace(&config, &ns);
                    let cfg_for_predicate = config.clone();
                    let ns_for_predicate = ns.clone();
                    let predicate = move |meta: &session::SessionMeta| -> bool {
                        daily_log_catchup_predicate(meta, &cfg_for_predicate, &ns_for_predicate)
                    };
                    catchup_pending_daily_logs(
                        &channel_session_store,
                        Some(&serve_state.acp_session_store),
                        provider.as_ref(),
                        &ws_state,
                        &workspace_dir,
                        &ns,
                        config.day_boundary_hour,
                        &predicate,
                    )
                    .await;
                    catchup_missing_daily_digests(
                        provider.as_ref(),
                        &ws_state,
                        &workspace_dir,
                        &ns,
                    )
                    .await;
                    if config.digest.weekly_enabled {
                        catchup_pending_weekly_logs(
                            provider.as_ref(),
                            &ws_state,
                            &workspace_dir,
                            &ns,
                            today_local,
                        )
                        .await;
                    }
                    if config.digest.monthly_enabled {
                        catchup_pending_monthly_logs(
                            provider.as_ref(),
                            &ws_state,
                            &workspace_dir,
                            &ns,
                            today_local,
                        )
                        .await;
                    }
                    if config.digest.yearly_enabled {
                        catchup_pending_yearly_logs(
                            provider.as_ref(),
                            &ws_state,
                            &workspace_dir,
                            &ns,
                            today_local,
                        )
                        .await;
                    }
                }
            }

            if config.heartbeat_enabled {
                let default_room_id = config
                    .matrix
                    .as_ref()
                    .and_then(|m| m.primary_room_id().map(str::to_string))
                    .or_else(|| {
                        config
                            .discord
                            .as_ref()
                            .and_then(|d| d.channel_ids.first().cloned())
                    });
                Heartbeat {
                    workspace_dir: workspace_dir.clone(),
                    ws_state: Arc::clone(&ws_state),
                    day_boundary_hour: config.day_boundary_hour,
                    daily_log_enabled: config.daily_log_enabled,
                    memory_compaction_enabled: config.memory_compaction_enabled,
                    digest_cfg: config.digest.clone(),
                    session_store: Arc::clone(&channel_session_store),
                    registry: Arc::clone(&registry),
                    agent: channel_agent.clone(),
                    default_room_id,
                    placement: Arc::clone(&placement),
                    local_rooms: channel::seed_routing_from_config(&config),
                    local_catch_all: config
                        .matrix
                        .as_ref()
                        .is_some_and(|m| m.room_ids.is_empty())
                        || config
                            .discord
                            .as_ref()
                            .is_some_and(|d| d.channel_ids.is_empty()),
                    config: config.clone(),
                    serve_state: Some(Arc::clone(&serve_state)),
                }
                .spawn();
            } else {
                tracing::info!("Heartbeat disabled by config");
            }

            // ── HTTP API server ─────────────────────────────────────────────
            // Opt-in: only `[serve]` or `--bind` opens a port. Local clients
            // (`chat`, `acp`, the desktop) use the IPC endpoint, so an agent
            // without one still works on its own host — and two agents on
            // one host no longer collide on the default port.
            let addr = bind.or_else(|| {
                config
                    .serve
                    .as_ref()
                    .map(|s| format!("{}:{}", s.host, s.port))
            });
            if addr.is_none() {
                for (what, on) in [
                    (
                        "[acp] (GET /acp)",
                        config.acp.as_ref().is_some_and(|c| c.enabled),
                    ),
                    (
                        "[a2a] (/a2a)",
                        config.a2a.as_ref().is_some_and(|c| c.enabled),
                    ),
                    ("ambient ingest (/audio/*)", ambient_routes.is_some()),
                ] {
                    if on {
                        tracing::warn!(
                            "{what} is enabled but unreachable: the HTTP API is off. Add a \
                             [serve] table to {} to listen.",
                            config_path.display()
                        );
                    }
                }
            }

            let served = serve::run(
                addr,
                Arc::clone(&serve_state),
                ambient_routes,
                shutdown.clone(),
            )
            .await;
            // However the HTTP server ended, everything else stops with it.
            shutdown.cancel();
            switch_watch.abort();

            // Wait for the agent task's graceful shutdown to finish so its
            // flush_digests_on_shutdown LLM call isn't aborted by runtime drop.
            if let Some(handle) = agent_handle
                && let Err(e) = handle.await
            {
                tracing::warn!("Agent task did not finish cleanly: {e}");
            }
            served?;

            if restart.load(Ordering::SeqCst) {
                // The app server is still serving the new workspace; the
                // process exit takes it down with the rest, and the service
                // manager starts everything again around the new one.
                tracing::info!("exiting with {} to restart", app_server::RESTART_EXIT_CODE);
                std::process::exit(app_server::RESTART_EXIT_CODE);
            }
            server_task.await.context("the app server panicked")??;
        }
        Command::Verify => {
            let mut profile_errors = config.validate_profiles();
            // `serve` refuses to start on what only the registry can see —
            // a profile naming `anthropic` with no API key, or no key and no
            // `[profiles.default]` — so `verify` builds it too.
            if profile_errors.is_empty()
                && let Err(e) = ProviderRegistry::from_config(&config)
            {
                profile_errors.push(format!("{e:#}"));
            }
            if profile_errors.is_empty() {
                println!("Config OK");
            } else {
                println!("Config INVALID:");
                for err in &profile_errors {
                    println!("  - {err}");
                }
            }
            for (file, keys) in group_unknown_keys(
                &unknown,
                &provenance,
                &config_path,
                workspace_path.as_deref(),
            ) {
                println!("  Unknown keys      : {file}");
                for key in keys {
                    println!("    {key} (ignored)");
                }
            }
            // The device -> room_profile binding is written by hand, so it
            // needs somewhere to be checked.
            match device_auth::DeviceAuth::bridge(&config.room_profiles) {
                Ok(auth) => verify_external_devices(&auth).await,
                Err(e) => println!("  Devices           : INVALID: {e:#}"),
            }
            match &workspace_path {
                Some(p) => println!("  Workspace config  : {}", p.display()),
                None => {
                    let ws_config = config::workspace_config_path(&workspace_dir);
                    println!(
                        "  Workspace config  : none ({} absent)",
                        ws_config.display()
                    );
                }
            }
            if let Some(m) = &config.matrix {
                println!("  Channel           : matrix");
                println!("  Matrix homeserver : {}", m.homeserver);
                println!("  Matrix user_id    : {}", m.user_id);
                println!("  Matrix rooms      : {:?}", m.room_ids);
            } else if let Some(d) = &config.discord {
                println!("  Channel           : discord");
                println!("  Discord channels  : {:?}", d.channel_ids);
                println!("  Allowed users     : {:?}", d.allowed_users);
            } else {
                println!("  Channel           : NONE (add [discord] or [matrix] to config)");
            }
            match &config.serve {
                Some(s) => println!("  HTTP API          : http://{}:{}", s.host, s.port),
                None => println!("  HTTP API          : disabled (add [serve] to listen)"),
            }
            println!(
                "  ACP endpoint      : {}",
                if config.acp.as_ref().is_some_and(|c| c.enabled) {
                    "enabled (GET /acp)"
                } else {
                    "disabled"
                }
            );
            println!(
                "  Anthropic model   : {} [{}]",
                config.anthropic.model,
                layer_tag(&provenance, "anthropic.model")
            );
            println!(
                "  Anthropic max_tok : {} [{}]",
                config.anthropic.max_tokens,
                layer_tag(&provenance, "anthropic.max_tokens")
            );
            match &selection {
                Some(_) => println!(
                    "  Workspace dir     : {} (selected)",
                    workspace_dir.display()
                ),
                None => println!(
                    "  Workspace dir     : {} (from `workspace_dir`; selected on the next start)",
                    workspace_dir.display()
                ),
            }
            println!(
                "  Day boundary hour : {}:00 local [{}]",
                config.day_boundary_hour,
                layer_tag(&provenance, "day_boundary_hour")
            );
            println!(
                "  Heartbeat enabled : {} [{}]",
                config.heartbeat_enabled,
                layer_tag(&provenance, "heartbeat_enabled")
            );
            println!();
            println!("Workspace-supplied settings:");
            let ws_settings: Vec<&String> = provenance
                .iter()
                .filter(|(_, layer)| **layer == config_layer::Layer::Workspace)
                .map(|(path, _)| path)
                .collect();
            if ws_settings.is_empty() {
                println!("  (none)");
            } else {
                for path in ws_settings {
                    println!("  {path}");
                }
            }
            println!();
            let workspace_files = [
                ("AGENTS.md / AGENT.md", vec!["AGENTS.md", "AGENT.md"]),
                ("SOUL.md", vec!["SOUL.md"]),
                ("IDENTITY.md", vec!["IDENTITY.md"]),
                ("USER.md", vec!["USER.md"]),
                ("TOOLS.md", vec!["TOOLS.md"]),
                ("BOOTSTRAP.md", vec!["BOOTSTRAP.md"]),
                ("MEMORY.md", vec!["MEMORY.md", "memory.md"]),
            ];
            for (label, candidates) in &workspace_files {
                let found = candidates.iter().find_map(|f| {
                    let p = workspace_dir.join(f);
                    if p.exists() { Some(*f) } else { None }
                });
                match found {
                    Some(f) => println!("  {label:<28} found ({f})"),
                    None => println!("  {label:<28} -"),
                }
            }
        }
        Command::Framework(_) => {
            unreachable!("the framework's verbs are dispatched ahead of the config load")
        }
    }

    Ok(())
}

/// Migrate flat `sessions/<kind>/<uuid>.jsonl` layouts to the namespaced
/// `sessions/<namespace>/<kind>/<uuid>.jsonl` form. Each session's
/// namespace is read from `meta.namespace` (newer files), falling back
/// to `"default"` for legacy files that predate that field.
///
/// Idempotent: skips silently when no flat `sessions/<kind>/*.jsonl`
/// files exist. Refuses to run when a target path is already occupied
/// (extremely unlikely with ULID/UUID file names but handled for
/// safety).
fn migrate_sessions_to_namespaced_layout(sessions_base: &std::path::Path) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};

    // Pre-namespace migration covers both the legacy `"api"` kind dir and
    // the post-#112 `"rpc"` dir; either may exist in a long-lived workspace
    // that skipped a release.
    for kind in ["channel", "api", "rpc"] {
        let kind_dir = sessions_base.join(kind);
        if !kind_dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&kind_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Skipping {}: {e}", kind_dir.display());
                continue;
            }
        };
        let mut moved = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Read just the first line to extract meta.namespace.
            let namespace = match std::fs::File::open(&path) {
                Ok(f) => {
                    let mut first = String::new();
                    let _ = BufReader::new(f).read_line(&mut first);
                    serde_json::from_str::<serde_json::Value>(first.trim())
                        .ok()
                        .and_then(|v| {
                            v.get("meta")
                                .and_then(|m| m.get("namespace"))
                                .and_then(|n| n.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| config::DEFAULT_NAMESPACE_NAME.to_string())
                }
                Err(_) => config::DEFAULT_NAMESPACE_NAME.to_string(),
            };

            let Some(file_name) = path.file_name() else {
                continue;
            };
            let dst_dir = sessions_base.join(&namespace).join(kind);
            std::fs::create_dir_all(&dst_dir)
                .with_context(|| format!("create {}", dst_dir.display()))?;
            let dst = dst_dir.join(file_name);
            if dst.exists() {
                anyhow::bail!(
                    "Refusing to migrate {}: destination {} already exists. \
                     Reconcile manually before starting.",
                    path.display(),
                    dst.display(),
                );
            }
            std::fs::rename(&path, &dst)
                .with_context(|| format!("rename {} -> {}", path.display(), dst.display()))?;
            moved += 1;
        }
        // Remove the now-empty flat directory; non-fatal if it still has
        // unexpected leftovers (e.g. user dropped files).
        let _ = std::fs::remove_dir(&kind_dir);
        if moved > 0 {
            tracing::info!(
                "Migrated {} session file(s) from {} into namespaced layout",
                moved,
                kind_dir.display()
            );
        }
    }
    Ok(())
}

/// One-shot migration completing #112 + #122: split each
/// `sessions/<ns>/{api,rpc}/` directory into the new layout.
///
/// - Files whose name starts with `voice-` are pre-redesign voice
///   sessions. Their `(device_id, room_profile)` was hashed into the
///   filename and is unrecoverable from disk, so we can't slot them
///   into the new device-default layout. They move to
///   `sessions/<ns>/legacy-voice/` instead — the daily-log archive
///   still indexes their content for FTS / semantic search; only the
///   live continuation is given up.
/// - Everything else moves to `sessions/<ns>/cross-device/`, with
///   stored `meta.channel = "api"` rewritten to `"rpc"` in-place.
/// - The PR 1 dual-read shim and dual-accept fallbacks become
///   unnecessary after this run.
///
/// Idempotent: a workspace that's already migrated has no `api/` or
/// `rpc/` directory under any namespace and the function exits silently.
/// Refuses to run when a name collision would clobber an existing
/// destination (extremely unlikely with UUIDs but handled for safety).
fn migrate_to_device_default_layout(sessions_base: &std::path::Path) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let Ok(ns_entries) = std::fs::read_dir(sessions_base) else {
        return Ok(());
    };

    let mut moved = 0usize;
    let mut quarantined = 0usize;

    for ns_entry in ns_entries.flatten() {
        let ns_dir = ns_entry.path();
        if !ns_dir.is_dir() {
            continue;
        }

        for src_kind in ["api", "rpc"] {
            let src_dir = ns_dir.join(src_kind);
            if !src_dir.is_dir() {
                continue;
            }
            let entries = match std::fs::read_dir(&src_dir) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", src_dir.display());
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(file_name) = path.file_name() else {
                    continue;
                };
                let is_voice = file_name
                    .to_str()
                    .map(|s| s.starts_with("voice-"))
                    .unwrap_or(false);

                let (dst_dir, needs_meta_rewrite) = if is_voice {
                    (ns_dir.join("legacy-voice"), false)
                } else {
                    // Cross-device files moving from the legacy `api/`
                    // directory still carry `meta.channel = "api"` in
                    // their first line — rewrite to `"rpc"` so the
                    // dual-accept fallbacks can be removed.
                    (ns_dir.join("cross-device"), src_kind == "api")
                };
                std::fs::create_dir_all(&dst_dir)
                    .with_context(|| format!("create {}", dst_dir.display()))?;
                let dst = dst_dir.join(file_name);
                if dst.exists() {
                    anyhow::bail!(
                        "Refusing to migrate {}: destination {} already exists. \
                         Reconcile manually before starting.",
                        path.display(),
                        dst.display(),
                    );
                }

                if needs_meta_rewrite {
                    // Stream the source into a temp file at the
                    // destination dir (so `fs::rename` stays
                    // intra-filesystem and atomic), rewriting just the
                    // first line. Then drop the source.
                    let tmp = dst.with_extension("partial");
                    let src_file = std::fs::File::open(&path)
                        .with_context(|| format!("open {}", path.display()))?;
                    let mut reader = BufReader::new(src_file);
                    let mut first_line = String::new();
                    reader
                        .read_line(&mut first_line)
                        .with_context(|| format!("read first line of {}", path.display()))?;

                    let rewritten = rewrite_meta_channel(first_line.trim(), "rpc")?;
                    let mut writer = std::fs::File::create(&tmp)
                        .with_context(|| format!("create {}", tmp.display()))?;
                    writeln!(writer, "{rewritten}")
                        .with_context(|| format!("write {}", tmp.display()))?;
                    std::io::copy(&mut reader, &mut writer)
                        .with_context(|| format!("copy body of {}", path.display()))?;
                    drop(writer);
                    std::fs::rename(&tmp, &dst).with_context(|| {
                        format!("rename {} -> {}", tmp.display(), dst.display())
                    })?;
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove {}", path.display()))?;
                } else {
                    std::fs::rename(&path, &dst).with_context(|| {
                        format!("rename {} -> {}", path.display(), dst.display())
                    })?;
                }

                if is_voice {
                    quarantined += 1;
                } else {
                    moved += 1;
                }
            }
            // Remove the now-empty source dir; non-fatal if it has
            // leftovers (e.g. operator dropped unrelated files).
            let _ = std::fs::remove_dir(&src_dir);
        }
    }

    if moved > 0 || quarantined > 0 {
        tracing::info!(
            "Session migration: {moved} cross-device session(s), \
             {quarantined} legacy voice file(s) quarantined under legacy-voice/"
        );
    }
    Ok(())
}

/// Parse a session-meta JSONL line, set `meta.channel` to `new_channel`,
/// and re-serialise. Used by `migrate_to_device_default_layout` to
/// rewrite legacy `"api"` strings to `"rpc"` while preserving all
/// other meta fields.
fn rewrite_meta_channel(line: &str, new_channel: &str) -> anyhow::Result<String> {
    let mut value: serde_json::Value =
        serde_json::from_str(line).with_context(|| format!("parse meta line: {line}"))?;
    if let Some(meta) = value.get_mut("meta").and_then(|m| m.as_object_mut()) {
        meta.insert(
            "channel".to_string(),
            serde_json::Value::String(new_channel.to_string()),
        );
    } else {
        anyhow::bail!("first JSONL line missing `meta` object: {line}");
    }
    Ok(serde_json::to_string(&value)?)
}

/// One-shot migration from the per-channel session layout
/// (`sessions/matrix/` + `sessions/discord/`) to the consolidated
/// `sessions/channel/` directory. The originating channel is still
/// recorded in each session's `meta.channel` field, so the move is
/// pure reshuffling.
///
/// Idempotent: skips silently when neither legacy directory exists.
/// Refuses to run when a name collision would clobber an existing
/// `sessions/channel/<id>.jsonl` (extremely unlikely with ULIDs but
/// handled for safety).
fn migrate_per_channel_sessions(sessions_base: &std::path::Path) -> anyhow::Result<()> {
    let target = sessions_base.join("channel");
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    for legacy in ["matrix", "discord"] {
        let dir = sessions_base.join(legacy);
        if dir.is_dir() {
            sources.push(dir);
        }
    }
    if sources.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;

    for src in sources {
        let entries = match std::fs::read_dir(&src) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Skipping {}: {e}", src.display());
                continue;
            }
        };
        let mut moved = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name() else {
                continue;
            };
            let dst = target.join(file_name);
            if dst.exists() {
                anyhow::bail!(
                    "Refusing to migrate {}: destination {} already exists. \
                     Reconcile manually before starting.",
                    path.display(),
                    dst.display(),
                );
            }
            std::fs::rename(&path, &dst)
                .with_context(|| format!("rename {} -> {}", path.display(), dst.display()))?;
            moved += 1;
        }
        // Remove the now-empty legacy directory; non-fatal if it has
        // unexpected leftover entries (e.g. user-dropped files).
        let _ = std::fs::remove_dir(&src);
        tracing::info!(
            "Migrated {} session file(s) from {} to {}",
            moved,
            src.display(),
            target.display()
        );
    }
    Ok(())
}

/// One-shot migration from the pre-namespace `memory/{daily,weekly,monthly,
/// yearly}/` layout to the namespaced `memory/default/{...}` layout. Also
/// moves a top-level MEMORY.md (or lowercase memory.md) into
/// `memory/default/MEMORY.md`.
///
/// Idempotent: skips silently when `memory/default/` already exists with
/// no top-level periodic dirs to move. Refuses to run when both old and
/// new layouts coexist (ambiguous merge — the user must reconcile).
fn migrate_pre_namespace_layout(workspace_dir: &std::path::Path) -> anyhow::Result<()> {
    let memory_root = workspace_dir.join("memory");
    let has_top_memory_md = ["MEMORY.md", "memory.md"]
        .iter()
        .any(|f| workspace_dir.join(f).is_file());
    if !memory_root.is_dir() && !has_top_memory_md {
        return Ok(());
    }

    let default_root = memory_root.join(config::DEFAULT_NAMESPACE_NAME);
    let kinds = ["daily", "weekly", "monthly", "yearly"];
    let pre_dirs: Vec<_> = kinds
        .iter()
        .filter(|k| memory_root.join(k).is_dir())
        .copied()
        .collect();
    let pre_memory_md = ["MEMORY.md", "memory.md"]
        .iter()
        .map(|f| workspace_dir.join(f))
        .find(|p| p.is_file());

    if pre_dirs.is_empty() && pre_memory_md.is_none() {
        return Ok(());
    }

    if default_root.is_dir() && !pre_dirs.is_empty() {
        anyhow::bail!(
            "Both pre-namespace memory dirs ({}) and memory/{} exist — refuse to migrate. \
             Reconcile manually before starting.",
            pre_dirs.join(", "),
            config::DEFAULT_NAMESPACE_NAME,
        );
    }

    std::fs::create_dir_all(&default_root)
        .with_context(|| format!("create {}", default_root.display()))?;

    for kind in &pre_dirs {
        let src = memory_root.join(kind);
        let dst = default_root.join(kind);
        std::fs::rename(&src, &dst)
            .with_context(|| format!("rename {} -> {}", src.display(), dst.display()))?;
        tracing::info!(
            "Migrated memory subdir: {} -> {}",
            src.display(),
            dst.display()
        );
    }

    if let Some(src) = pre_memory_md {
        let dst = default_root.join("MEMORY.md");
        std::fs::rename(&src, &dst)
            .with_context(|| format!("rename {} -> {}", src.display(), dst.display()))?;
        tracing::info!("Migrated MEMORY.md: {} -> {}", src.display(), dst.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acp_meta(namespace: &str) -> session::SessionMeta {
        session::SessionMeta {
            session_id: "s1".to_string(),
            // ACP sessions have no room; project_meta sets this empty
            // for real (acp_session.rs), so an empty room_id must not
            // accidentally make the predicate fall through to
            // room-derived namespace resolution.
            room_id: String::new(),
            thread_id: None,
            channel: "acp".to_string(),
            created_at: chrono::Utc::now(),
            public_id: None,
            namespace: Some(namespace.to_string()),
            device_id: None,
            room_profile: None,
            title: None,
        }
    }

    /// One warning per file, with the file named from provenance: a
    /// message that says only "unknown key `x.y`" leaves the person
    /// grepping two files for it.
    #[test]
    fn unknown_keys_are_grouped_under_the_file_that_supplied_them() {
        let provenance = std::collections::BTreeMap::from([
            (
                "room_profile.default.skills".to_string(),
                config_layer::Layer::Host,
            ),
            (
                "memory_namespace.dev.skils".to_string(),
                config_layer::Layer::Workspace,
            ),
        ]);
        let unknown = vec![
            "memory_namespace.dev.skils".to_string(),
            "room_profile.default.skills".to_string(),
        ];

        let grouped = group_unknown_keys(
            &unknown,
            &provenance,
            std::path::Path::new("/etc/host.toml"),
            Some(std::path::Path::new("/ws/.sapphire-agent/config.toml")),
        );

        assert_eq!(
            grouped,
            vec![
                (
                    "/etc/host.toml".to_string(),
                    vec!["room_profile.default.skills"]
                ),
                (
                    "/ws/.sapphire-agent/config.toml".to_string(),
                    vec!["memory_namespace.dev.skils"]
                ),
            ]
        );
    }

    /// A key attributed to the workspace layer when no workspace file was
    /// read cannot name a file, and must not fall through to the host path
    /// — that would point at a file the key is not in.
    #[test]
    fn an_unattributable_unknown_key_names_no_file() {
        let unknown = ["stray".to_string()];
        let grouped = group_unknown_keys(
            &unknown,
            &std::collections::BTreeMap::new(),
            std::path::Path::new("/etc/host.toml"),
            None,
        );
        assert_eq!(
            grouped,
            vec![("the configuration".to_string(), vec!["stray"])]
        );
    }

    #[test]
    fn acp_session_is_excluded_by_a_predicate_for_a_different_namespace() {
        let config = config::Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        let meta = acp_meta("work");
        assert!(!daily_log_catchup_predicate(
            &meta,
            &config,
            config::DEFAULT_NAMESPACE_NAME
        ));
    }

    #[test]
    fn acp_session_is_included_by_a_predicate_for_its_own_namespace() {
        let config = config::Config::parse_for_test("[anthropic]\napi_key = \"test\"\n");
        let meta = acp_meta("work");
        assert!(daily_log_catchup_predicate(&meta, &config, "work"));
    }

    #[test]
    fn device_add_requires_a_name() {
        assert!(Cli::try_parse_from(["sapphire-agent", "device", "add"]).is_err());
    }

    #[test]
    fn external_device_add_takes_a_name_and_a_description() {
        use sapphire_framework::server::FrameworkCommand;
        use sapphire_framework_bridge_api::ExternalDeviceCommand;
        let cli = Cli::try_parse_from([
            "sapphire-agent",
            "external-device",
            "add",
            "pendant",
            "--description",
            "the one on the lanyard",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Framework(FrameworkCommand::ExternalDevice(
                ExternalDeviceCommand::Add {
                    ref name,
                    description: Some(_),
                    ..
                }
            )) if name == "pendant"
        ));
    }

    /// The framework's verbs sit beside the agent's own at the top level,
    /// and a bare invocation prints the help.
    #[test]
    fn the_framework_verbs_parse_beside_the_agents() {
        use sapphire_framework::server::{FrameworkCommand, WorkspaceCommand};
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;

        let err = Cli::try_parse_from(["sapphire-agent"])
            .err()
            .expect("a bare invocation does not parse");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert!(matches!(
            parse(&["sapphire-agent", "serve"]),
            Command::Framework(FrameworkCommand::Serve)
        ));
        assert!(matches!(
            parse(&["sapphire-agent", "status"]),
            Command::Framework(FrameworkCommand::Status)
        ));
        assert!(matches!(
            parse(&["sapphire-agent", "workspace", "show"]),
            Command::Framework(FrameworkCommand::Workspace(WorkspaceCommand::Show))
        ));
        assert!(matches!(
            parse(&["sapphire-agent", "verify"]),
            Command::Verify
        ));
        assert!(matches!(
            parse(&["sapphire-agent", "init"]),
            Command::Init { .. }
        ));
        assert!(matches!(parse(&["sapphire-agent", "acp"]), Command::Acp));
        assert!(matches!(
            parse(&["sapphire-agent", "chat", "--session", "a3b7k9p", "-m", "hi", "--json"]),
            Command::Chat {
                session: Some(ref s),
                message: Some(ref m),
                json: true,
                list: false,
                history: false,
            } if s == "a3b7k9p" && m == "hi"
        ));
    }

    /// With ring and aws-lc-rs both compiled in, building a rustls config
    /// without naming a provider panics unless one has been installed.
    #[test]
    fn a_default_crypto_provider_is_installed() {
        install_crypto_provider();
        let _ = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }

    fn write_stub(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn migration_no_op_on_fresh_workspace() {
        let td = tempfile::tempdir().unwrap();
        migrate_pre_namespace_layout(td.path()).expect("clean migration");
        // Nothing created.
        assert!(!td.path().join("memory").exists());
    }

    #[test]
    fn migration_moves_pre_namespace_layout() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_stub(&root.join("memory/daily/2026-04-15.md"), "daily body");
        write_stub(&root.join("memory/weekly/2026-W16.md"), "weekly body");
        write_stub(&root.join("memory/monthly/2026-04.md"), "monthly body");
        write_stub(&root.join("memory/yearly/2026.md"), "yearly body");
        write_stub(&root.join("MEMORY.md"), "core memory");

        migrate_pre_namespace_layout(root).expect("migration");

        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/daily/2026-04-15.md")).unwrap(),
            "daily body"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/weekly/2026-W16.md")).unwrap(),
            "weekly body"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/monthly/2026-04.md")).unwrap(),
            "monthly body"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/yearly/2026.md")).unwrap(),
            "yearly body"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/MEMORY.md")).unwrap(),
            "core memory"
        );
        // Old paths gone.
        assert!(!root.join("memory/daily").exists());
        assert!(!root.join("MEMORY.md").exists());
    }

    #[test]
    fn migration_accepts_lowercase_memory_md() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_stub(&root.join("memory.md"), "lowercase memory");

        migrate_pre_namespace_layout(root).expect("migration");

        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/MEMORY.md")).unwrap(),
            "lowercase memory"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write_stub(&root.join("memory/daily/2026-04-15.md"), "daily body");
        migrate_pre_namespace_layout(root).expect("first migration");
        // Run again — pre-namespace dirs no longer exist; default tree
        // already in place.
        migrate_pre_namespace_layout(root).expect("idempotent migration");
        assert_eq!(
            std::fs::read_to_string(root.join("memory/default/daily/2026-04-15.md")).unwrap(),
            "daily body"
        );
    }

    #[test]
    fn migration_refuses_when_layouts_coexist() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        // Both old and new layouts present — ambiguous.
        write_stub(&root.join("memory/daily/2026-04-15.md"), "old");
        write_stub(&root.join("memory/default/daily/2026-04-16.md"), "new");

        let err = migrate_pre_namespace_layout(root).expect_err("error");
        assert!(format!("{err:#}").contains("Reconcile manually"));
        // Files untouched.
        assert!(root.join("memory/daily/2026-04-15.md").exists());
        assert!(root.join("memory/default/daily/2026-04-16.md").exists());
    }

    #[test]
    fn session_migration_no_op_on_fresh_workspace() {
        let td = tempfile::tempdir().unwrap();
        migrate_per_channel_sessions(td.path()).expect("clean migration");
        assert!(!td.path().join("channel").exists());
    }

    #[test]
    fn session_migration_consolidates_matrix_and_discord() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        write_stub(&base.join("matrix/01H1.jsonl"), "matrix");
        write_stub(&base.join("matrix/01H2.jsonl"), "matrix2");
        write_stub(&base.join("discord/01H3.jsonl"), "discord");

        migrate_per_channel_sessions(base).expect("migration");

        for stem in ["01H1", "01H2", "01H3"] {
            let path = base.join("channel").join(format!("{stem}.jsonl"));
            assert!(path.exists(), "missing: {}", path.display());
        }
        assert!(
            !base.join("matrix").exists(),
            "matrix dir should be removed"
        );
        assert!(
            !base.join("discord").exists(),
            "discord dir should be removed"
        );
    }

    #[test]
    fn session_migration_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        write_stub(&base.join("matrix/01H1.jsonl"), "matrix");
        migrate_per_channel_sessions(base).expect("first");
        migrate_per_channel_sessions(base).expect("second is a no-op");
        assert!(base.join("channel/01H1.jsonl").exists());
    }

    #[test]
    fn session_migration_refuses_on_collision() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        write_stub(&base.join("matrix/01H1.jsonl"), "matrix");
        write_stub(&base.join("channel/01H1.jsonl"), "pre-existing");
        let err = migrate_per_channel_sessions(base).expect_err("error");
        assert!(
            format!("{err:#}").contains("already exists"),
            "got: {err:#}"
        );
        // Original files untouched.
        assert_eq!(
            std::fs::read_to_string(base.join("matrix/01H1.jsonl")).unwrap(),
            "matrix"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("channel/01H1.jsonl")).unwrap(),
            "pre-existing"
        );
    }

    // ── device-default layout migration (#122 PR 3) ──────────────────────

    /// Render a minimal meta line for the device-default migration tests.
    /// Only `session_id` and `channel` are exercised by the migration —
    /// everything else just rides along untouched.
    fn meta_line(session_id: &str, channel: &str) -> String {
        format!(
            r#"{{"meta":{{"session_id":"{session_id}","room_id":"","thread_id":null,"channel":"{channel}","created_at":"2026-05-22T00:00:00Z","namespace":"default"}}}}"#
        )
    }

    #[test]
    fn device_default_migration_no_op_on_fresh_workspace() {
        let td = tempfile::tempdir().unwrap();
        migrate_to_device_default_layout(td.path()).expect("clean migration");
        // No directories should be created on an empty workspace.
        assert!(!td.path().join("default").exists());
    }

    /// Cross-device text sessions move into `cross-device/`;
    /// `voice-<hash>` files go into `legacy-voice/`. Both source dirs
    /// are removed afterwards.
    #[test]
    fn device_default_migration_moves_cross_device_and_quarantines_voice() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        let ns = base.join("default");
        write_stub(
            &ns.join("api").join("uuid-text.jsonl"),
            &meta_line("uuid-text", "api"),
        );
        write_stub(
            &ns.join("rpc").join("uuid-text-modern.jsonl"),
            &meta_line("uuid-text-modern", "rpc"),
        );
        write_stub(
            &ns.join("api").join("voice-deadbeef.jsonl"),
            &meta_line("voice-deadbeef", "api"),
        );
        write_stub(
            &ns.join("rpc").join("voice-feedface.jsonl"),
            &meta_line("voice-feedface", "rpc"),
        );

        migrate_to_device_default_layout(base).expect("migration");

        assert!(ns.join("cross-device/uuid-text.jsonl").exists());
        assert!(ns.join("cross-device/uuid-text-modern.jsonl").exists());
        assert!(ns.join("legacy-voice/voice-deadbeef.jsonl").exists());
        assert!(ns.join("legacy-voice/voice-feedface.jsonl").exists());
        assert!(!ns.join("api").exists(), "api dir should be removed");
        assert!(!ns.join("rpc").exists(), "rpc dir should be removed");
    }

    /// Files that arrive from the legacy `api/` dir get their stored
    /// `meta.channel = "api"` rewritten to `"rpc"`; files from `rpc/`
    /// keep their meta unchanged. Voice files in `legacy-voice/` are
    /// preserved verbatim (no rewrite — they're never re-loaded).
    #[test]
    fn device_default_migration_rewrites_legacy_meta_channel() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        let ns = base.join("default");
        write_stub(
            &ns.join("api").join("uuid-from-api.jsonl"),
            &meta_line("uuid-from-api", "api"),
        );
        write_stub(
            &ns.join("rpc").join("uuid-from-rpc.jsonl"),
            &meta_line("uuid-from-rpc", "rpc"),
        );
        write_stub(
            &ns.join("api").join("voice-cafe.jsonl"),
            &meta_line("voice-cafe", "api"),
        );

        migrate_to_device_default_layout(base).expect("migration");

        let api_migrated =
            std::fs::read_to_string(ns.join("cross-device/uuid-from-api.jsonl")).unwrap();
        assert!(
            api_migrated.contains(r#""channel":"rpc""#),
            "legacy `api` meta must be rewritten to `rpc`: {api_migrated}"
        );
        assert!(
            !api_migrated.contains(r#""channel":"api""#),
            "legacy `api` meta should no longer appear: {api_migrated}"
        );

        let rpc_migrated =
            std::fs::read_to_string(ns.join("cross-device/uuid-from-rpc.jsonl")).unwrap();
        assert!(
            rpc_migrated.contains(r#""channel":"rpc""#),
            "post-PR1 `rpc` meta stays as-is: {rpc_migrated}"
        );

        let voice_quarantined =
            std::fs::read_to_string(ns.join("legacy-voice/voice-cafe.jsonl")).unwrap();
        assert!(
            voice_quarantined.contains(r#""channel":"api""#),
            "voice meta is preserved verbatim (no rewrite): {voice_quarantined}"
        );
    }

    #[test]
    fn device_default_migration_is_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        let ns = base.join("default");
        write_stub(
            &ns.join("api").join("uuid-text.jsonl"),
            &meta_line("uuid-text", "api"),
        );
        migrate_to_device_default_layout(base).expect("first");
        migrate_to_device_default_layout(base).expect("second is a no-op");
        assert!(ns.join("cross-device/uuid-text.jsonl").exists());
    }

    #[test]
    fn device_default_migration_refuses_on_collision() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        let ns = base.join("default");
        write_stub(
            &ns.join("api").join("uuid-text.jsonl"),
            &meta_line("uuid-text", "api"),
        );
        write_stub(
            &ns.join("cross-device").join("uuid-text.jsonl"),
            "pre-existing",
        );

        let err = migrate_to_device_default_layout(base).expect_err("collision");
        assert!(
            format!("{err:#}").contains("already exists"),
            "got: {err:#}"
        );
        // Source untouched.
        assert!(ns.join("api/uuid-text.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(ns.join("cross-device/uuid-text.jsonl")).unwrap(),
            "pre-existing"
        );
    }
}
