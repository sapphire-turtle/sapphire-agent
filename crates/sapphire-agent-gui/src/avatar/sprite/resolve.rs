//! Which sprite shows, and what the 2D avatar declares. Pure: the caller
//! says which files loaded, this decides.

// Task 2/3 (the loader and the renderer) consume these; until then they are
// only exercised by the tests.
#![allow(dead_code)]

use std::collections::BTreeSet;

use bevy::math::UVec2;
use sapphire_agent_core::{AvatarCaps, SpriteConfig};

/// What to draw: a pose's base image and, optionally, an expression overlay.
/// `base` and `overlay` are avatar paths, as written in the config.
#[derive(Debug, PartialEq)]
pub(crate) struct Shown<'a> {
    pub pose: &'a str,
    pub base: &'a str,
    pub overlay: Option<&'a str>,
}

/// The pose to draw (the requested one, else the default) and the overlay for
/// `expression`. `None` when neither pose has a loaded base.
pub(crate) fn shown<'a>(
    config: &'a SpriteConfig,
    loaded: &impl Fn(&str) -> bool,
    pose: Option<&str>,
    expression: &str,
) -> Option<Shown<'a>> {
    let usable = |name: &str| {
        config
            .poses
            .get_key_value(name)
            .filter(|(_, p)| loaded(&p.base))
    };
    let (name, p) = pose
        .and_then(usable)
        .or_else(|| usable(&config.default_pose))?;
    let overlay = if expression == "neutral" {
        None
    } else {
        p.expressions
            .get(expression)
            .map(String::as_str)
            .filter(|path| loaded(path))
    };
    Some(Shown {
        pose: name,
        base: &p.base,
        overlay,
    })
}

/// What the avatar can do, given what loaded. `None` when the default pose's
/// base did not load, so there is nothing to declare.
pub(crate) fn declared(
    config: &SpriteConfig,
    loaded: &impl Fn(&str) -> bool,
) -> Option<AvatarCaps> {
    let default = config.poses.get(&config.default_pose)?;
    if !loaded(&default.base) {
        return None;
    }
    let poses = config
        .poses
        .iter()
        .filter(|(_, p)| loaded(&p.base))
        .map(|(name, _)| name.clone())
        .collect();
    let expressions: BTreeSet<&str> = config
        .poses
        .values()
        .flat_map(|p| &p.expressions)
        .filter(|(_, path)| loaded(path))
        .map(|(key, _)| key.as_str())
        .collect();
    Some(AvatarCaps {
        expressions: std::iter::once("neutral")
            .chain(expressions)
            .map(String::from)
            .collect(),
        actions: Vec::new(),
        poses,
    })
}

/// The shared canvas size. The first size wins; the error names the first
/// image that differs.
pub(crate) fn same_canvas(sizes: &[(String, UVec2)]) -> Result<UVec2, String> {
    let Some((_, first)) = sizes.first() else {
        return Err("no images".into());
    };
    match sizes.iter().find(|(_, size)| size != first) {
        Some((path, size)) => Err(format!(
            "{path} is {}×{}, the others are {}×{}",
            size.x, size.y, first.x, first.y
        )),
        None => Ok(*first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_agent_core::SpritePose;
    use std::collections::BTreeMap;

    fn config() -> SpriteConfig {
        let pose = |base: &str, ex: &str, img: &str| SpritePose {
            base: base.into(),
            expressions: BTreeMap::from([(ex.to_string(), img.to_string())]),
        };
        SpriteConfig {
            default_pose: "normal".into(),
            poses: BTreeMap::from([
                ("normal".to_string(), pose("n.png", "happy", "nh.png")),
                ("arms".to_string(), pose("a.png", "sad", "as.png")),
            ]),
        }
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn shown_uses_the_pose_and_its_overlay() {
        let c = config();
        let s = shown(&c, &all, Some("arms"), "sad").unwrap();
        assert_eq!(
            (s.pose, s.base, s.overlay),
            ("arms", "a.png", Some("as.png"))
        );
    }

    #[test]
    fn an_unknown_pose_falls_back_to_the_default() {
        let c = config();
        let s = shown(&c, &all, Some("gone"), "happy").unwrap();
        assert_eq!(
            (s.pose, s.base, s.overlay),
            ("normal", "n.png", Some("nh.png"))
        );
    }

    #[test]
    fn an_expression_the_pose_lacks_shows_the_base() {
        let c = config();
        assert_eq!(
            shown(&c, &all, Some("arms"), "happy").unwrap().overlay,
            None
        );
    }

    #[test]
    fn neutral_has_no_overlay() {
        let c = config();
        assert_eq!(shown(&c, &all, None, "neutral").unwrap().overlay, None);
    }

    #[test]
    fn an_unloaded_overlay_is_dropped() {
        let c = config();
        let loaded = |p: &str| p != "nh.png";
        assert_eq!(shown(&c, &loaded, None, "happy").unwrap().overlay, None);
    }

    #[test]
    fn no_default_base_shows_nothing() {
        let c = config();
        let loaded = |p: &str| p != "n.png";
        assert!(shown(&c, &loaded, None, "neutral").is_none());
        assert!(declared(&c, &loaded).is_none());
    }

    #[test]
    fn declared_lists_loaded_poses_and_expressions() {
        let c = config();
        let loaded = |p: &str| p != "as.png";
        let caps = declared(&c, &loaded).unwrap();
        assert_eq!(caps.poses, ["arms", "normal"]);
        assert_eq!(caps.expressions, ["neutral", "happy"]);
        assert!(caps.actions.is_empty());
    }

    #[test]
    fn same_canvas_reports_the_first_mismatch() {
        let s = |p: &str, w, h| (p.to_string(), UVec2::new(w, h));
        let err = same_canvas(&[s("a", 64, 128), s("b", 64, 128), s("c", 32, 32)]).unwrap_err();
        assert!(err.contains("c is 32×32"), "{err}");
        assert!(err.contains("64×128"), "{err}");
        assert_eq!(same_canvas(&[s("a", 64, 128)]), Ok(UVec2::new(64, 128)));
    }
}
