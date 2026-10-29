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
//! which owns the chat, its connection to the agent running on this host and
//! the egui setup. What stays here is what only a desktop has: logging, the
//! window and the camera the GUI draws onto.

use bevy::prelude::*;
use clap::Parser;
use sapphire_agent_cli::{Command, Options};
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
        Some(command) => sapphire_agent_cli::run(cli.options, command),
        None => {
            window();
            Ok(())
        }
    }
}

fn window() {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("warn,sapphire_agent_desktop=info,sapphire_agent_gui=info")
        }))
        .init();

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "sapphire-agent-desktop".to_string(),
                resolution: (640u32, 720u32).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(AgentGuiPlugin {
            app: AppIdentity {
                app_name: "sapphire-agent",
                version: env!("CARGO_PKG_VERSION"),
            },
        })
        .add_systems(Startup, spawn_camera)
        .run();
}

/// bevy_egui renders onto a camera-bound surface — without an active
/// camera the window paints black and no UI shows up. A plain `Camera2d`
/// is enough until we introduce the 3D avatar (swap for Camera3d then).
fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}
