//! The framework's settings (workspace, devices, appearance, about…) and
//! the theme and language they hold, applied to the whole GUI.
//!
//! [`GuiSettings`] holds sapphire-framework-gui's [`Settings`] over a
//! [`FrameworkClient`] on the GUI's runtime. Every frame, whether or not
//! the settings are open, [`follow`] registers the framework's strings,
//! polls the client and applies the theme and language the settings hold,
//! so a change made on another device restyles the chat too.
//!
//! The look last applied is kept on the device by corundum's store
//! ([`attach_look_store`]), so the window opens in it before the bridge
//! answers, or without one.

use bevy::prelude::*;
use bevy_egui::EguiContexts;
use corundum_framework_gui::{look, store};
use sapphire_framework_gui::client::{AppIdentity, ClientConfig, FrameworkClient};
use sapphire_framework_gui::{Health, Settings};
use tracing::{info, warn};

use crate::app::Waker;

/// The app name corundum's store is kept under: the desktop's own, so it
/// doesn't share a file with the agent CLI.
const LOOK_STORE_APP: &str = "sapphire-agent-desktop";

/// The framework's settings and whether they are shown in place of the
/// chat. A non-`Send` resource: `Settings` holds the app's sections as
/// plain trait objects.
pub struct GuiSettings {
    pub settings: Settings,
    /// Shows the settings in place of the chat. The top bar's button
    /// toggles it.
    pub open: bool,
}

impl GuiSettings {
    /// Settings over a client for `app`, refreshing on `runtime` and waking
    /// the GUI with `waker`. `None`, logged, when the client cannot be
    /// configured: the chat works without the settings.
    pub(crate) fn spawn(
        app: AppIdentity,
        runtime: &tokio::runtime::Handle,
        waker: Waker,
    ) -> Option<GuiSettings> {
        let config = match ClientConfig::new(app) {
            Ok(config) => config,
            Err(err) => {
                warn!("cannot configure the framework client; the settings are unavailable: {err}");
                return None;
            }
        };
        let client = FrameworkClient::spawn(runtime, config, waker);
        Some(GuiSettings {
            settings: Settings::new(client),
            open: false,
        })
    }
}

/// Which screen fills the window below the top bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    Chat,
    Settings,
}

/// The button at the top bar's end that switches screens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Toggle {
    OpenSettings,
    BackToChat,
}

/// What the frame shows, decided from the settings' state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) screen: Screen,
    /// `None` without settings.
    pub(crate) toggle: Option<Toggle>,
    /// The services' status banner under the session line.
    pub(crate) banner: bool,
}

impl Layout {
    /// The layout for settings that are `open` (or not) with `health`, or
    /// `None` when there are no settings.
    pub(crate) fn of(settings: Option<(bool, Health)>) -> Layout {
        match settings {
            None => Layout {
                screen: Screen::Chat,
                toggle: None,
                banner: false,
            },
            Some((true, _)) => Layout {
                screen: Screen::Settings,
                toggle: Some(Toggle::BackToChat),
                // Settings on its own draws the banner above its pages.
                banner: false,
            },
            Some((false, health)) => Layout {
                screen: Screen::Chat,
                toggle: Some(Toggle::OpenSettings),
                banner: health == Health::Problem,
            },
        }
    }
}

/// Keeps the look in corundum's store and puts back the one saved there,
/// so the window opens in the last look before the bridge answers. A
/// one-shot on the first `EguiPrimaryContextPass`, as the fonts are.
pub(crate) fn attach_look_store(mut contexts: EguiContexts, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    *done = true;
    let Some(look_store) = store::Store::for_app(LOOK_STORE_APP) else {
        warn!("no config directory for {LOOK_STORE_APP}; the look is not kept across restarts");
        return;
    };
    let path = look_store.path().to_path_buf();
    if store::attach(ctx, look_store) {
        info!("put back the look kept in {}", path.display());
    }
    if let Some(saved) = store::state(ctx).look {
        look::apply(ctx, &saved);
    }
}

/// Every frame, before the screens: the framework's strings, the
/// settings' outcomes, and the theme and language they hold.
pub(crate) fn follow(mut contexts: EguiContexts, settings: Option<NonSendMut<GuiSettings>>) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    sapphire_framework_gui::i18n::register(ctx, sapphire_framework_gui::RESOURCES);
    let Some(mut settings) = settings else {
        return;
    };
    settings.settings.poll();
    settings.settings.follow_theme(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_settings_only_the_chat_shows() {
        assert_eq!(
            Layout::of(None),
            Layout {
                screen: Screen::Chat,
                toggle: None,
                banner: false,
            }
        );
    }

    #[test]
    fn closed_settings_offer_to_open_them() {
        assert_eq!(
            Layout::of(Some((false, Health::Healthy))),
            Layout {
                screen: Screen::Chat,
                toggle: Some(Toggle::OpenSettings),
                banner: false,
            }
        );
    }

    #[test]
    fn open_settings_replace_the_chat_and_offer_the_way_back() {
        assert_eq!(
            Layout::of(Some((true, Health::Healthy))),
            Layout {
                screen: Screen::Settings,
                toggle: Some(Toggle::BackToChat),
                banner: false,
            }
        );
    }

    #[test]
    fn a_problem_shows_the_banner_on_the_chat() {
        assert!(Layout::of(Some((false, Health::Problem))).banner);
    }

    #[test]
    fn open_settings_draw_their_own_banner() {
        // Settings on its own shows the banner above its pages.
        assert!(!Layout::of(Some((true, Health::Problem))).banner);
    }

    #[test]
    fn only_a_problem_shows_the_banner() {
        for health in [Health::Connecting, Health::Healthy, Health::UpdateAvailable] {
            assert!(!Layout::of(Some((false, health))).banner, "{health:?}");
        }
    }
}
