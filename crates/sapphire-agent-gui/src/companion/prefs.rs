//! The companion's preferences on this device (spec §9): how to draw the
//! avatar, when it reverts, whether the gesture buttons show and where the
//! window was. Kept in `companion.toml` in the app's config directory and
//! never synced.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};

use crate::avatar::DisplayMode;

/// The shortest and longest revert times the prefs accept, in seconds.
const REVERT_SECS: std::ops::RangeInclusive<u32> = 5..=600;

/// Where the companion's window was, in physical pixels on the desktop.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The companion's preferences on this device.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CompanionPrefs {
    /// How to draw the avatar (spec §14).
    pub display_mode: DisplayMode,
    /// Seconds after a turn's reply before expression and pose go back to
    /// their defaults (spec §8); 5–600.
    pub revert_secs: u32,
    /// Shows input, history and menu buttons beside the character, for
    /// those who cannot use the gestures (spec §5).
    pub show_buttons: bool,
    /// Where the window was last; `None` until it has been placed.
    pub window: Option<WindowPlacement>,
}

impl Default for CompanionPrefs {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::Auto,
            revert_secs: 30,
            show_buttons: false,
            window: None,
        }
    }
}

impl CompanionPrefs {
    /// `companion.toml` in the app's config directory, if the platform has
    /// one.
    pub fn default_path() -> Option<PathBuf> {
        Some(
            corundum_framework_platform::dirs::AppDirs::new(sapphire_agent_core::APP)?
                .config()
                .join("companion.toml"),
        )
    }

    /// The prefs in `path`, or the defaults (logged) when it is missing or
    /// cannot be read. Out-of-range values are brought into range.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no {}; the companion uses its defaults", path.display());
                return Self::default();
            }
            Err(e) => {
                tracing::warn!("cannot read {}; using the defaults: {e}", path.display());
                return Self::default();
            }
        };
        let mut prefs: Self = match toml::from_str(&text) {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!("cannot parse {}; using the defaults: {e}", path.display());
                return Self::default();
            }
        };
        prefs.revert_secs = prefs
            .revert_secs
            .clamp(*REVERT_SECS.start(), *REVERT_SECS.end());
        prefs
    }

    /// Writes the prefs to `path` through a temporary file, so a crash
    /// mid-write leaves the old file whole.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string(self).context("serialising the companion prefs")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_clamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("companion.toml");

        let prefs = CompanionPrefs {
            display_mode: DisplayMode::Sprite,
            revert_secs: 45,
            show_buttons: true,
            window: Some(WindowPlacement {
                x: -20,
                y: 40,
                width: 400,
                height: 600,
            }),
        };
        prefs.save(&path).unwrap();
        assert_eq!(CompanionPrefs::load(&path), prefs);

        std::fs::write(&path, "revert_secs = 1\n").unwrap();
        let loaded = CompanionPrefs::load(&path);
        assert_eq!(loaded.revert_secs, 5);
        assert_eq!(loaded.display_mode, DisplayMode::Auto);

        std::fs::write(&path, "revert_secs = 100000\n").unwrap();
        assert_eq!(CompanionPrefs::load(&path).revert_secs, 600);

        std::fs::write(&path, "this is = = not toml").unwrap();
        assert_eq!(CompanionPrefs::load(&path), CompanionPrefs::default());

        assert_eq!(
            CompanionPrefs::load(&dir.path().join("missing.toml")),
            CompanionPrefs::default()
        );
    }

    #[test]
    fn defaults_follow_the_spec() {
        let d = CompanionPrefs::default();
        assert_eq!(d.display_mode, DisplayMode::Auto);
        assert_eq!(d.revert_secs, 30);
        assert!(!d.show_buttons);
        assert_eq!(d.window, None);
    }
}
