//! The 3D VRM renderer.
//!
//! Owned by plan 4-3D (`docs/superpowers/plans/2026-10-31-desktop-companion-4-3d-vrm.md`):
//! that plan fills in this directory and nothing outside it. It follows the
//! renderer contract in [`super`]. Changes to shared code (anything outside
//! this directory) go through `main` first (spec §14), not through the
//! renderer's branch.
//!
//! `build` already adds `bevy_vrm1`'s [`VrmPlugin`] and [`VrmaPlugin`]
//! (plan 4-0), but only when bevy's [`GltfPlugin`] is in the app: they need
//! the asset server, the shader assets and glTF's extension handlers, and
//! `VrmPlugin` asserts that `GltfPlugin` was added. A host adds
//! `DefaultPlugins` first, so they are there; the GUI's headless tests run
//! the avatar under `MinimalPlugins`, without them, and the renderer then
//! draws nothing.

use bevy::gltf::GltfPlugin;
use bevy::prelude::*;
use bevy_vrm1::prelude::{VrmPlugin, VrmaPlugin};

/// Draws the avatar from its VRM model while `ActiveRenderer` names the VRM
/// renderer. Empty until plan 4-3D, apart from `bevy_vrm1`'s plugins.
pub struct VrmRendererPlugin;

impl Plugin for VrmRendererPlugin {
    fn build(&self, app: &mut App) {
        // Without glTF (a `MinimalPlugins` app) bevy_vrm1's plugins would
        // panic; see the module docs.
        if app.is_plugin_added::<GltfPlugin>() {
            app.add_plugins((VrmPlugin, VrmaPlugin));
        } else {
            tracing::debug!("no GltfPlugin in the app: the VRM renderer is not loaded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bevy_vrm1_is_skipped_without_gltf() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, VrmRendererPlugin));
        app.update();
        assert!(app.is_plugin_added::<VrmRendererPlugin>());
        assert!(!app.is_plugin_added::<VrmPlugin>());
        assert!(!app.is_plugin_added::<VrmaPlugin>());
    }
}
