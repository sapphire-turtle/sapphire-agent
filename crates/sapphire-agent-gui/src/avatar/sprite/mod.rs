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

mod load;
mod resolve;

/// Draws the avatar from its sprite sheet while `ActiveRenderer` names the
/// sprite renderer.
pub struct SpriteRendererPlugin;

impl Plugin for SpriteRendererPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<load::SpriteImages>()
            // `WindowPlugin` adds it too; an app without one still runs.
            .add_message::<RequestRedraw>()
            .add_systems(
                Update,
                (load::load_sprites, load::settle_sprites)
                    .chain()
                    .after(AvatarSystems::Choose)
                    .run_if(renderer_is(ActiveRenderer::Sprite)),
            );
    }
}
