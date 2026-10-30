//! sapphire-agent-desktop entry point.
//!
//! A thin shell around [`sapphire_agent_gui::AgentGuiPlugin`], which owns
//! the chat, its connection to the agent running on this host and the egui
//! setup and the cameras. What stays here is what only a desktop has:
//! logging and the companion's window ([`companion_window`]).

mod companion_window;
mod global_pointer;

use bevy::prelude::*;
use sapphire_agent_gui::{AgentGuiPlugin, AppIdentity};
use tracing_subscriber::{EnvFilter, fmt};

fn main() {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("warn,sapphire_agent_desktop=info,sapphire_agent_gui=info")
        }))
        .init();

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
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
        })
        .add_plugins(companion_window::CompanionWindowPlugin)
        .run();
}
