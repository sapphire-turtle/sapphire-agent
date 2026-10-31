//! The 2D sprite renderer.
//!
//! Owned by plan 4-2D (`docs/superpowers/plans/2026-10-31-desktop-companion-4-2d-sprite.md`):
//! that plan fills in this directory and nothing outside it. It follows the
//! renderer contract in [`super`]. Changes to shared code (anything outside
//! this directory) go through `main` first (spec §14), not through the
//! renderer's branch.

use bevy::prelude::*;

/// Draws the avatar from its sprite sheet while `ActiveRenderer` names the
/// sprite renderer. Empty until plan 4-2D.
pub struct SpriteRendererPlugin;

impl Plugin for SpriteRendererPlugin {
    fn build(&self, _app: &mut App) {}
}
