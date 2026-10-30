//! `sapphire-agent`: the agent's command line.
//!
//! As `serve`, it is the app server and the agent around it
//! (`sapphire-agent-server`). The client commands (`chat`, `acp`, and the
//! framework's `status`, `workspace`, …) talk to the server already running on
//! this host over its IPC endpoint and start none.
//!
//! The command line is a library so both editions run the same commands
//! ([`run`]): the headless one (`sapphire-agent-headless`) and the desktop
//! app, which runs them when given a subcommand. Both put `sapphire-agent` on
//! PATH.

pub mod chat;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use sapphire_framework::server::FrameworkCommand;
use tracing_subscriber::{EnvFilter, fmt};

pub use sapphire_agent_server::Edition;

#[derive(Parser)]
#[command(
    name = "sapphire-agent",
    version,
    arg_required_else_help = true,
    about = "Personal AI assistant — Anthropic + Matrix/Discord",
    // Without this, `--help` shows the flattened `FrameworkCommand`'s doc
    // comment as the long description instead of `about`.
    long_about = None
)]
pub struct Cli {
    #[command(flatten)]
    pub options: Options,

    #[command(subcommand)]
    pub command: Command,
}

/// The options before the subcommand, which `serve` and `verify` read.
#[derive(Args)]
pub struct Options {
    /// Path to config file (default: ~/.config/sapphire-agent/config.toml)
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Listen for HTTP on this address (e.g. 127.0.0.1:3170), overriding
    /// `[serve]`. Without either, `serve` opens no HTTP port.
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
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
    Framework(FrameworkCommand),
}

/// Run one command as `edition`, exiting the process with its code when a
/// framework verb fails with one.
pub fn run(options: Options, command: Command, edition: Edition) -> Result<()> {
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

    sapphire_agent_server::install_crypto_provider();
    sapphire_agent_server::init_app_ctx();

    // `init` is what an operator runs when there is no host config yet, so
    // it needs neither the config nor a runtime.
    if let Command::Init { path } = command {
        return sapphire_agent_server::init_workspace(path);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(dispatch(options, command, edition))
}

async fn dispatch(options: Options, command: Command, edition: Edition) -> Result<()> {
    match command {
        Command::Init { .. } => unreachable!("`init` is dispatched ahead of the runtime"),
        // A relay to the running agent, which holds the config: nothing to
        // load here, and stdout belongs to ACP.
        Command::Acp => sapphire_agent_server::acp_stdio().await,
        // Likewise a client of the running agent.
        Command::Chat {
            session,
            list,
            message,
            history,
            json,
        } => {
            chat::run(chat::ChatOptions {
                session,
                list,
                message,
                history,
                json,
            })
            .await
        }
        Command::Verify => sapphire_agent_server::verify(options.config).await,
        // What the service manager starts.
        Command::Framework(FrameworkCommand::Serve) => {
            sapphire_agent_server::serve(options.config, options.bind, edition).await
        }
        // The framework's other verbs talk to the running server (which
        // relays the bridge's), and need no agent config either.
        Command::Framework(command) => {
            // Warned first: the dispatch exits when the status is not good.
            if matches!(command, FrameworkCommand::Status) {
                warn_if_shadowed();
            }
            // The agent has no keys of its own in the framework's settings
            // registry (`()`): its `config.toml` tables are its own to read.
            let code = command
                .dispatch(
                    &sapphire_agent_server::APP_CTX,
                    (),
                    |_: &sapphire_framework::server::Settings| {
                        sapphire_agent_server::build(edition)
                    },
                    sapphire_agent_server::VERSION,
                )
                .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

/// The command both editions put on PATH.
const COMMAND: &str = "sapphire-agent";

/// Warn when the `sapphire-agent` on PATH is another executable than this
/// one: with both the headless edition and the desktop app installed, the one a shell or an
/// ACP/MCP client runs may not be the one the service was registered with.
fn warn_if_shadowed() {
    let Ok(this) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return;
    };
    let Some(on_path) = std::env::var_os("PATH").and_then(|path| find_on_path(&path)) else {
        return;
    };
    if std::fs::canonicalize(&on_path).is_ok_and(|found| found != this) {
        eprintln!(
            "warning: `{COMMAND}` on PATH is {}, not this executable ({}); \
             keep only one of the headless edition and the desktop app installed",
            on_path.display(),
            this.display(),
        );
    }
}

/// The first `sapphire-agent` executable in `path`, a `PATH` value.
fn find_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    let file = format!("{COMMAND}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path)
        .map(|dir| dir.join(&file))
        .find(|candidate| Path::is_file(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_framework::server::WorkspaceCommand;
    use sapphire_framework_bridge_api::ExternalDeviceCommand;

    #[test]
    fn version_flag_prints_the_version() {
        let err = Cli::try_parse_from(["sapphire-agent", "--version"])
            .err()
            .expect("--version stops the parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string(),
            format!("sapphire-agent {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn device_add_requires_a_name() {
        assert!(Cli::try_parse_from(["sapphire-agent", "device", "add"]).is_err());
    }

    #[test]
    fn external_device_add_takes_a_name_and_a_description() {
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
}
