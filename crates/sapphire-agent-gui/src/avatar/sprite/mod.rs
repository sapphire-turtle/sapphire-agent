//! The 2D sprite renderer.
//!
//! Owned by plan 4-2D (`docs/superpowers/plans/2026-10-31-desktop-companion-4-2d-sprite.md`):
//! that plan fills in this directory and nothing outside it. It follows the
//! renderer contract in [`super`]. Changes to shared code (anything outside
//! this directory) go through `main` first (spec §14), not through the
//! renderer's branch.

use bevy::prelude::*;
use bevy::window::RequestRedraw;

use super::{ActiveRenderer, AvatarSystems, renderer_is};
use crate::companion::character_page::RendererPanelApp;

mod draw;
mod load;
mod panel;
mod resolve;

/// Draws the avatar from its sprite sheet while `ActiveRenderer` names the
/// sprite renderer.
pub struct SpriteRendererPlugin;

impl Plugin for SpriteRendererPlugin {
    fn build(&self, app: &mut App) {
        app.add_renderer_panel(Box::new(panel::SpritePanel::default()))
            .init_resource::<load::SpriteImages>()
            .init_resource::<draw::SpriteScene>()
            // `WindowPlugin` adds it too; an app without one still runs.
            .add_message::<RequestRedraw>()
            .add_systems(
                Update,
                (
                    // Ungated: it runs on the switch away.
                    draw::despawn_sprite_scene.run_if(resource_changed::<ActiveRenderer>),
                    (
                        load::load_sprites,
                        load::settle_sprites,
                        draw::show_sprites,
                        draw::fade_sprites,
                        draw::place_sprites,
                    )
                        .chain()
                        .run_if(renderer_is(ActiveRenderer::Sprite)),
                )
                    .chain()
                    .after(AvatarSystems::Choose),
            )
            .add_systems(
                PostUpdate,
                draw::write_bounds
                    .in_set(AvatarSystems::Bounds)
                    .run_if(renderer_is(ActiveRenderer::Sprite)),
            );
    }
}
