//! Which renderer draws the avatar: the display mode the user picked and
//! the sections the avatar's config has decide it (spec §14).
//!
//! [`choose`] is a pure function; [`renderer_is`] is the run condition
//! each renderer's systems carry, so only the chosen one runs.

use bevy::prelude::{Res, Resource};
use sapphire_agent_core::AvatarConfig;
use serde::{Deserialize, Serialize};

/// The user's choice of how to draw the avatar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayMode {
    /// 3D when the avatar has a model, else 2D.
    #[default]
    Auto,
    /// The `[vrm]` model only.
    Vrm,
    /// The `[sprite]` images only.
    Sprite,
}

/// The renderer drawing the avatar now.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveRenderer {
    /// No avatar to draw: a neutral silhouette stands in.
    Placeholder,
    Vrm,
    Sprite,
}

/// The renderer for `mode` given the avatar's `config`; the placeholder
/// when there is no avatar or it lacks the section the mode asks for.
pub fn choose(mode: DisplayMode, config: Option<&AvatarConfig>) -> ActiveRenderer {
    let Some(config) = config else {
        return ActiveRenderer::Placeholder;
    };
    let (vrm, sprite) = (config.vrm.is_some(), config.sprite.is_some());
    match mode {
        DisplayMode::Auto | DisplayMode::Vrm if vrm => ActiveRenderer::Vrm,
        DisplayMode::Auto | DisplayMode::Sprite if sprite => ActiveRenderer::Sprite,
        _ => ActiveRenderer::Placeholder,
    }
}

/// Run condition: `r` is the active renderer.
pub fn renderer_is(r: ActiveRenderer) -> impl Fn(Res<ActiveRenderer>) -> bool {
    move |active: Res<ActiveRenderer>| *active == r
}

#[cfg(test)]
mod tests {
    use sapphire_agent_core::{AvatarConfig, SpriteConfig, VrmConfig};

    use super::*;

    fn config(vrm: bool, sprite: bool) -> AvatarConfig {
        AvatarConfig {
            vrm: vrm.then(|| VrmConfig {
                model: "a.vrm".into(),
                ..Default::default()
            }),
            sprite: sprite.then(|| SpriteConfig {
                default_pose: "p".into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn choose_follows_the_spec_table() {
        use ActiveRenderer as R;
        use DisplayMode as M;
        let both = config(true, true);
        let vrm = config(true, false);
        let sprite = config(false, true);

        assert_eq!(choose(M::Auto, Some(&both)), R::Vrm);
        assert_eq!(choose(M::Auto, Some(&sprite)), R::Sprite);
        assert_eq!(choose(M::Auto, Some(&vrm)), R::Vrm);
        assert_eq!(choose(M::Sprite, Some(&vrm)), R::Placeholder);
        assert_eq!(choose(M::Vrm, Some(&sprite)), R::Placeholder);
        assert_eq!(choose(M::Vrm, Some(&both)), R::Vrm);
        assert_eq!(choose(M::Sprite, Some(&both)), R::Sprite);
        for mode in [M::Auto, M::Vrm, M::Sprite] {
            assert_eq!(choose(mode, None), R::Placeholder);
        }
        assert_eq!(
            choose(M::Auto, Some(&AvatarConfig::default())),
            R::Placeholder
        );
    }
}
