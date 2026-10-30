//! sapphire-agent-desktop entry point.
//!
//! A thin shell around [`sapphire_agent_gui::AgentGuiPlugin`], which owns
//! the chat, its connection to the agent running on this host and the egui
//! setup and the cameras. What stays here is what only a desktop has:
//! logging and the window.

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
        .run();
}
