//! The GUI of sapphire-agent, shared by the desktop app and a future
//! mobile one.
//!
//! [`AgentGuiPlugin`] is the whole of it: the app adds bevy's
//! `DefaultPlugins`, its window and a camera, then this plugin. It brings
//! egui ([`bevy_egui::EguiPlugin`]), a tokio runtime for the agent client
//! ([`GuiRuntime`]), a [`Waker`] that lets that runtime wake bevy's event
//! loop, and the chat ([`chat`]).
//!
//! Window concerns (size, decorations, platform quirks) stay in the app;
//! nothing here is `cfg(target_os)`.

mod app;
pub mod chat;

use bevy::prelude::*;
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};

pub use app::{GuiRuntime, GuiWaker, Waker};
pub use sapphire_framework_gui::client::AppIdentity;

/// The GUI as one bevy plugin. Add it after `DefaultPlugins`.
pub struct AgentGuiPlugin {
    /// The app the GUI runs in. Its version is the one the chat reports
    /// to the agent.
    pub app: AppIdentity,
}

impl Plugin for AgentGuiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default())
            // Idle while nothing happens: input, egui's own repaint
            // requests and the `Waker` are what bring the next frame.
            .insert_resource(bevy::winit::WinitSettings::desktop_app())
            .add_systems(Update, chat::drain_bridge_events)
            .add_systems(
                EguiPrimaryContextPass,
                (app::setup_fonts, app::setup_image_loaders, chat::view::ui).chain(),
            )
            .add_systems(
                Last,
                app::shutdown_runtime.in_set(bevy::app::OnAppExitSystems),
            );
    }

    /// The runtime, the waker and the bridge are made here rather than in
    /// `build` so the waker finds winit's event-loop proxy whichever order
    /// the plugins were added in.
    fn finish(&self, app: &mut App) {
        let runtime = app::build_runtime();
        let waker = app::event_loop_waker(app.world());
        let (bridge, events) = chat::RpcBridge::new(runtime.handle().clone(), waker.clone());
        let bridge = bridge.client_version(self.app.version);

        // Connect right away so the chat screen has a session by the time
        // the window paints.
        bridge.submit_connect();

        app.insert_resource(GuiRuntime(runtime))
            .insert_resource(GuiWaker(waker))
            .insert_resource(chat::ChatState::new())
            .insert_resource(chat::ChatBridge(bridge))
            .insert_resource(chat::BridgeEventQueue::new(events));
    }
}
