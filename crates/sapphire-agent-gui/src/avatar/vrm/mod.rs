//! The 3D VRM renderer.
//!
//! Owned by plan 4-3D (`docs/superpowers/plans/2026-10-31-desktop-companion-4-3d-vrm.md`):
//! that plan fills in this directory and nothing outside it, including
//! adding `bevy_vrm1::prelude::{VrmPlugin, VrmaPlugin}` to `build`. It
//! follows the renderer contract in [`super`]. Changes to shared code
//! (anything outside this directory) go through `main` first (spec §14),
//! not through the renderer's branch.

use bevy::prelude::*;

/// Draws the avatar from its VRM model while `ActiveRenderer` names the VRM
/// renderer. Empty until plan 4-3D.
pub struct VrmRendererPlugin;

impl Plugin for VrmRendererPlugin {
    fn build(&self, _app: &mut App) {}
}
