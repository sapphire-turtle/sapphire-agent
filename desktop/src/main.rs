//! sapphire-agent-desktop entry point: the window — and, given a subcommand,
//! the CLI.
//!
//! This one binary is a superset of the CLI's: bare, it opens the window;
//! with `serve`, `chat`, `acp`, `status`, … it runs the CLI's own code
//! (`sapphire_agent_cli::run`), so the service manager, editors and MCP
//! clients can start it as they start `sapphire-agent`. The subcommand is
//! dispatched before anything of bevy's starts.
//!
//! The window is a thin shell around [`sapphire_agent_gui::AgentGuiPlugin`],
//! which owns the chat, its connection to the agent running on this host,
//! the egui setup, the cameras and the avatar. What stays here is what only
//! a desktop has: logging and the window ([`companion_window`]; a
//! transparent companion).

mod companion_window;
mod file_dialog;
mod global_pointer;

use bevy::prelude::*;
use clap::Parser;
use sapphire_agent_cli::{Command, Edition, Options};
use sapphire_agent_gui::companion::FilePickerApp;
use sapphire_agent_gui::{AgentGuiPlugin, AppIdentity};
use tracing_subscriber::{EnvFilter, fmt};

/// The CLI's command line, with the subcommand optional: none opens the window.
#[derive(Parser)]
#[command(
    name = "sapphire-agent",
    about = "Personal AI assistant: the desktop app, or the CLI's commands",
    long_about = None,
    version
)]
struct DesktopCli {
    #[command(flatten)]
    options: Options,

    #[command(subcommand)]
    command: Option<Command>,
}

fn main() -> anyhow::Result<()> {
    let cli = DesktopCli::parse();
    match cli.command {
        Some(command) => sapphire_agent_cli::run(cli.options, command, Edition::Desktop),
        None => {
            window(cli.options.config);
            Ok(())
        }
    }
}

fn window(config: Option<std::path::PathBuf>) {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("warn,sapphire_agent_desktop=info,sapphire_agent_gui=info")
        }))
        .init();

    let mut app = App::new();
    // Before `DefaultPlugins`, whose `AssetPlugin` takes no source after.
    sapphire_agent_gui::avatar::register_asset_source(&mut app);
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(companion_window::window()),
        ..default()
    }))
    .add_plugins(AgentGuiPlugin {
        app: AppIdentity {
            app_name: "sapphire-agent",
            version: env!("CARGO_PKG_VERSION"),
        },
        // Idle while nothing happens: the global pointer wakes the
        // companion window when it matters.
        update_mode: None,
        // A new API key takes one of the agent's room profiles, read from
        // its config (and the selected workspace's) as the API keys page asks.
        key_profiles: Some({
            sapphire_agent_cli::init_app_ctx();
            sapphire_agent_cli::configured_key_profiles(config)
        }),
    })
    .add_plugins(companion_window::CompanionWindowPlugin)
    .set_file_dialog(std::sync::Arc::new(file_dialog::RfdDialog))
    .run();
}
