//! The "Character" page of the settings (spec §7, §9): how the companion
//! is drawn on this device, and the sections the renderers add for the
//! avatar's own files.
//!
//! [`Settings`](sapphire_framework_gui::Settings) owns its pages as plain
//! trait objects, so the page ([`CharacterPage`]) shares a
//! [`CharacterModel`] with bevy through a mutex. [`refresh`] copies the
//! resources into the model every frame (reading them only, so renderers
//! that watch them see no change) and hands back what the page edited.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bevy::prelude::*;
use bevy_egui::egui;
use sapphire_agent_core::AvatarConfig;
use sapphire_framework_gui::SettingsSection;

use super::{CompanionPrefs, FilePicker};
use crate::avatar::{
    ActiveRenderer, AvatarAssets, AvatarOps, DisplayMode, OpsStatus, RendererError,
};

/// A renderer's section of the Character page: the settings for the
/// avatar files it draws. A renderer plugin registers one in `build` with
/// [`RendererPanelApp::add_renderer_panel`], whatever order the plugins
/// are added in.
///
/// Change the avatar only through [`AvatarOps::apply`]: it uploads first,
/// edits the agent's current config rather than the possibly stale
/// [`PanelContext::config`], and reports under the panel's `source` name.
/// The page itself shows the renderer's error and every source's op
/// errors, so a panel need not repeat them.
pub trait RendererPanel: Send {
    /// The section's heading.
    fn title(&self) -> &str;
    fn ui(&mut self, ui: &mut egui::Ui, cx: &mut PanelContext<'_>);
}

/// What a [`RendererPanel`] sees and can do.
pub struct PanelContext<'a> {
    /// The avatar's config as the agent last had it, once synced.
    pub config: Option<&'a AvatarConfig>,
    /// Each avatar path the config references and the cached file holding
    /// its bytes.
    pub files: Option<&'a BTreeMap<String, PathBuf>>,
    /// Uploads and config changes to the agent: use [`AvatarOps::apply`].
    pub ops: &'a AvatarOps,
    /// Asks the user for a file through the host's dialog.
    pub file_picker: &'a FilePicker,
}

/// The renderers' panels, collected before the settings page takes them.
#[derive(Resource, Default)]
pub struct RendererPanels(Mutex<Vec<Box<dyn RendererPanel>>>);

impl RendererPanels {
    fn push(&self, panel: Box<dyn RendererPanel>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(panel);
    }

    pub(crate) fn take(&self) -> Vec<Box<dyn RendererPanel>> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Registers [`RendererPanel`]s on an [`App`].
pub trait RendererPanelApp {
    /// Adds `panel` to the Character page. Safe in any plugin order: the
    /// collection is created if it is not there yet.
    fn add_renderer_panel(&mut self, panel: Box<dyn RendererPanel>) -> &mut Self;
}

impl RendererPanelApp for App {
    fn add_renderer_panel(&mut self, panel: Box<dyn RendererPanel>) -> &mut Self {
        self.world_mut()
            .get_resource_or_init::<RendererPanels>()
            .push(panel);
        self
    }
}

/// The display mode whose button is marked "(in use)" while `active`
/// draws; none while the placeholder stands in.
fn in_use(active: ActiveRenderer) -> Option<DisplayMode> {
    match active {
        ActiveRenderer::Vrm => Some(DisplayMode::Vrm),
        ActiveRenderer::Sprite => Some(DisplayMode::Sprite),
        ActiveRenderer::Placeholder => None,
    }
}

/// What the page shows and edits, apart from egui and bevy.
pub(crate) struct CharacterModel {
    prefs: CompanionPrefs,
    edited: bool,
    config: Option<AvatarConfig>,
    files: Option<BTreeMap<String, PathBuf>>,
    generation: Option<u64>,
    /// The renderer's error.
    error: Option<String>,
    /// Why the latest sync of the avatar failed.
    sync_error: Option<String>,
    active: ActiveRenderer,
    /// `None` only in a `Default` model, which has no agent and no panels
    /// to draw; the page's model always has one.
    ops: Option<AvatarOps>,
    /// Likewise.
    file_picker: Option<FilePicker>,
    panels: Vec<Box<dyn RendererPanel>>,
}

impl Default for CharacterModel {
    fn default() -> Self {
        Self {
            prefs: CompanionPrefs::default(),
            edited: false,
            config: None,
            files: None,
            generation: None,
            error: None,
            sync_error: None,
            active: ActiveRenderer::Placeholder,
            ops: None,
            file_picker: None,
            panels: Vec::new(),
        }
    }
}

impl CharacterModel {
    pub(crate) fn new(
        ops: AvatarOps,
        file_picker: FilePicker,
        panels: Vec<Box<dyn RendererPanel>>,
    ) -> Self {
        Self {
            ops: Some(ops),
            file_picker: Some(file_picker),
            panels,
            ..Self::default()
        }
    }

    /// Changes the prefs as the page does; remembered until
    /// [`Self::take_prefs_edit`] if they differ.
    pub(crate) fn edit_prefs(&mut self, edit: impl FnOnce(&mut CompanionPrefs)) {
        let before = self.prefs.clone();
        edit(&mut self.prefs);
        if self.prefs != before {
            self.edited = true;
        }
    }

    /// The prefs as the page left them, once after each edit.
    pub(crate) fn take_prefs_edit(&mut self) -> Option<CompanionPrefs> {
        std::mem::take(&mut self.edited).then(|| self.prefs.clone())
    }

    /// Copies what the page displays from the resources. A pending edit
    /// is not overwritten by the prefs it came from.
    pub(crate) fn refresh(
        &mut self,
        prefs: &CompanionPrefs,
        assets: &AvatarAssets,
        error: Option<&str>,
        active: ActiveRenderer,
    ) {
        if !self.edited && self.prefs != *prefs {
            self.prefs = prefs.clone();
        }
        if self.generation != Some(assets.generation) {
            self.generation = Some(assets.generation);
            self.config = assets.loaded.as_ref().map(|l| l.config.clone());
            self.files = assets.loaded.as_ref().map(|l| l.files.clone());
        }
        if self.error.as_deref() != error {
            self.error = error.map(str::to_owned);
        }
        if self.sync_error != assets.error {
            self.sync_error.clone_from(&assets.error);
        }
        self.active = active;
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Character");

        let mut prefs = self.prefs.clone();
        let used = in_use(self.active);
        ui.label("Display mode");
        ui.horizontal(|ui| {
            for (mode, label) in [
                (DisplayMode::Auto, "Auto"),
                (DisplayMode::Vrm, "3D"),
                (DisplayMode::Sprite, "2D"),
            ] {
                ui.radio_value(&mut prefs.display_mode, mode, label);
                if used == Some(mode) {
                    ui.weak("(in use)");
                }
            }
        });
        ui.add(
            egui::Slider::new(&mut prefs.revert_secs, 5..=600)
                .suffix(" s")
                .text("Revert after"),
        );
        ui.checkbox(&mut prefs.show_buttons, "Show buttons");
        if prefs != self.prefs {
            self.edit_prefs(|p| *p = prefs);
            // An idle window draws no further frame to apply and save it.
            ui.ctx().request_repaint();
        }

        if let Some(error) = &self.sync_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Cannot fetch the character from the agent: {error}"),
            );
        }
        if let Some(error) = &self.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Cannot draw the character: {error}"),
            );
        }
        if let Some(ops) = &self.ops {
            let OpsStatus { busy, errors } = ops.status();
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Working…");
                });
            }
            for (source, error) in errors {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, format!("{source}: {error}"));
                    if ui.small_button("Dismiss").clicked() {
                        ops.dismiss(source);
                    }
                });
            }
        }

        ui.separator();
        let Self {
            config,
            files,
            ops,
            file_picker,
            panels,
            ..
        } = self;
        if panels.is_empty() {
            ui.weak("No renderer sections yet");
        }
        let (Some(ops), Some(file_picker)) = (ops.as_ref(), file_picker.as_ref()) else {
            return;
        };
        for panel in panels {
            ui.collapsing(panel.title().to_owned(), |ui| {
                let mut cx = PanelContext {
                    config: config.as_ref(),
                    files: files.as_ref(),
                    ops,
                    file_picker,
                };
                panel.ui(ui, &mut cx);
            });
        }
    }
}

/// The model [`CharacterPage`] and [`refresh`] share.
#[derive(Resource, Clone)]
pub(crate) struct CharacterShared(Arc<Mutex<CharacterModel>>);

impl CharacterShared {
    pub(crate) fn new(model: CharacterModel) -> Self {
        Self(Arc::new(Mutex::new(model)))
    }

    pub(crate) fn page(&self) -> CharacterPage {
        CharacterPage {
            model: Arc::clone(&self.0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CharacterModel> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The settings page.
pub(crate) struct CharacterPage {
    model: Arc<Mutex<CharacterModel>>,
}

impl SettingsSection for CharacterPage {
    fn label(&self, _ctx: &egui::Context) -> String {
        "Character".to_owned()
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        self.model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ui(ui);
    }
}

/// Every frame: hands the page's edits to the prefs and shows the page
/// the resources as they are. Only reads the resources the renderers
/// watch, and writes the prefs only when the page edited them.
pub(crate) fn refresh(
    shared: Res<CharacterShared>,
    mut prefs: ResMut<CompanionPrefs>,
    assets: Res<AvatarAssets>,
    error: Res<RendererError>,
    active: Res<ActiveRenderer>,
) {
    let mut model = shared.lock();
    if let Some(edited) = model.take_prefs_edit() {
        *prefs = edited;
    }
    model.refresh(&prefs, &assets, error.0.as_deref(), *active);
}

/// Turns an op's request for a sync into [`crate::chat::SyncRequested`].
pub(crate) fn request_sync_after_ops(
    ops: Res<AvatarOps>,
    mut requested: ResMut<crate::chat::SyncRequested>,
) {
    if ops.take_sync_request() {
        requested.0 = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_keeps_a_pending_edit_and_picks_up_new_prefs_when_idle() {
        let (assets, active) = (AvatarAssets::default(), ActiveRenderer::Placeholder);
        let mut model = CharacterModel::default();

        model.edit_prefs(|p| p.revert_secs = 90);
        let older = CompanionPrefs::default();
        model.refresh(&older, &assets, None, active);
        assert_eq!(model.prefs.revert_secs, 90, "the pending edit stays");
        assert!(model.take_prefs_edit().is_some());

        let newer = CompanionPrefs {
            show_buttons: true,
            ..CompanionPrefs::default()
        };
        model.refresh(&newer, &assets, None, active);
        assert_eq!(model.prefs, newer, "idle, the resource's prefs show");
        assert_eq!(model.take_prefs_edit(), None);
    }

    #[test]
    fn the_page_shows_why_the_avatar_did_not_sync() {
        let mut model = CharacterModel::default();
        let mut assets = AvatarAssets::default();
        assets.apply(Err(anyhow::anyhow!("offline")));
        model.refresh(
            &CompanionPrefs::default(),
            &assets,
            None,
            ActiveRenderer::Placeholder,
        );
        assert_eq!(model.sync_error.as_deref(), Some("offline"));

        assets.apply(Ok(crate::avatar::LoadedAvatar {
            config: Default::default(),
            files: Default::default(),
        }));
        model.refresh(
            &CompanionPrefs::default(),
            &assets,
            None,
            ActiveRenderer::Placeholder,
        );
        assert_eq!(model.sync_error, None, "a sync that succeeds clears it");
    }

    #[test]
    fn prefs_edited_on_the_page_reach_the_resource() {
        let mut model = CharacterModel::default();
        assert_eq!(model.take_prefs_edit(), None);

        model.edit_prefs(|p| p.revert_secs = 90);
        let edited = model.take_prefs_edit().expect("the edit is handed over");
        assert_eq!(edited.revert_secs, 90);
        assert_eq!(model.take_prefs_edit(), None, "once");
    }
}
