//! egui systems. Bevy-specific glue (resources, system signatures)
//! lives here; the underlying state model is the pure
//! [`crate::state`] module so it can move to a cross-platform
//! `sapphire-agent-gui` crate when mobile lands.

pub mod chat;
pub mod settings;

use bevy_egui::egui;

/// The `Ui` a screen lays its panels out in: the whole window, on the
/// background layer.
///
/// egui 0.36 shows panels inside a `Ui` rather than straight on the
/// `Context`, and bevy_egui hands systems only the context, so each frame
/// starts by making the root one — what egui's own integrations do.
pub(crate) fn root_ui(ctx: &egui::Context) -> egui::Ui {
    egui::Ui::new(
        ctx.clone(),
        "root".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.content_rect()),
    )
}
