//! The "2D" section of the Character page (spec §7): the sprite avatar's
//! poses, and the image in each pose's slots.
//!
//! The edits are pure functions over [`AvatarConfig`] so they test without
//! egui; the panel only wires them to buttons and to the file picker.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bevy::math::UVec2;
use bevy_egui::egui;
use sapphire_agent_core::{AvatarConfig, SPRITE_EXPRESSIONS, SpriteConfig, SpritePose};

use super::header::{file_size, image_info};
use super::load::LeftOut;
use super::resolve::same_canvas;
use crate::avatar::OpTicket;
use crate::companion::character_page::{PanelContext, RendererPanel};
use crate::companion::file_pick::{FileFilter, PickTicket};

/// The source name under which this panel's ops report their errors.
const SOURCE: &str = "2D";

const IMAGES: FileFilter = FileFilter {
    name: "PNG / WebP image",
    extensions: &["png", "webp"],
};

/// One image slot of a pose: the neutral base or an expression overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Base,
    Expression(&'static str),
}

impl Slot {
    /// The file stem and the label of the slot.
    fn key(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Expression(key) => key,
        }
    }
}

/// Points `slot` of `pose` at `path`. A base creates `[sprite]` (with
/// `pose` as the default) and the pose as needed; if the pose was removed
/// on another device meanwhile, uploading its base brings it back, which is
/// acceptable. An expression on a pose that is not there is an error: a
/// pose without a base is not a valid config. `vrm` is left alone.
pub(crate) fn with_image(
    config: &mut AvatarConfig,
    pose: &str,
    slot: Slot,
    path: String,
) -> Result<(), String> {
    match slot {
        Slot::Base => {
            let sprite = config.sprite.get_or_insert_with(|| SpriteConfig {
                default_pose: pose.to_owned(),
                poses: BTreeMap::new(),
            });
            sprite.poses.entry(pose.to_owned()).or_default().base = path;
        }
        Slot::Expression(key) => {
            let target = config
                .sprite
                .as_mut()
                .and_then(|s| s.poses.get_mut(pose))
                .ok_or_else(|| format!("there is no pose named {pose}"))?;
            target.expressions.insert(key.to_owned(), path);
        }
    }
    Ok(())
}

/// Why `name` cannot name a pose: it becomes a directory of the upload
/// path, which the agent checks (`validate_path`) as one path component.
fn name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("a pose needs a name")
    } else if name.contains(['/', '\\', ':']) {
        Some("a pose name cannot contain / \\ or :")
    } else if name.starts_with('.') || name.ends_with(".uploading") {
        // The agent refuses hidden and in-flight names (`is_transient`),
        // and `.`/`..` start with a dot.
        Some("a pose name cannot start with . or end in .uploading")
    } else {
        None
    }
}

/// Renames a pose; the default pose stays the default.
pub(crate) fn rename_pose(config: &mut AvatarConfig, from: &str, to: &str) -> Result<(), String> {
    if let Some(why) = name_error(to) {
        return Err(why.to_owned());
    }
    let sprite = config.sprite.as_mut().ok_or("there are no poses")?;
    if from != to && sprite.poses.contains_key(to) {
        return Err(format!("there is already a pose named {to}"));
    }
    let pose = sprite
        .poses
        .remove(from)
        .ok_or_else(|| format!("there is no pose named {from}"))?;
    sprite.poses.insert(to.to_owned(), pose);
    if sprite.default_pose == from {
        sprite.default_pose = to.to_owned();
    }
    Ok(())
}

/// Removes a pose from the config; its files stay. The default pose cannot
/// go until another one is the default.
pub(crate) fn remove_pose(config: &mut AvatarConfig, pose: &str) -> Result<(), String> {
    let sprite = config.sprite.as_mut().ok_or("there are no poses")?;
    if sprite.default_pose == pose {
        return Err("choose another default pose before deleting this one".to_owned());
    }
    sprite
        .poses
        .remove(pose)
        .map(|_| ())
        .ok_or_else(|| format!("there is no pose named {pose}"))
}

/// Makes `pose` the default.
pub(crate) fn set_default(config: &mut AvatarConfig, pose: &str) -> Result<(), String> {
    let sprite = config.sprite.as_mut().ok_or("there are no poses")?;
    if !sprite.poses.contains_key(pose) {
        return Err(format!("there is no pose named {pose}"));
    }
    sprite.default_pose = pose.to_owned();
    Ok(())
}

/// Refuses an image of `size` that is not the canvas size of `reference`
/// (the path and size of an image already in the avatar). With no
/// reference, the image sets the canvas.
pub(crate) fn check_canvas(
    reference: Option<(&str, UVec2)>,
    path: &str,
    size: UVec2,
) -> Result<(), String> {
    let Some((ref_path, ref_size)) = reference else {
        return Ok(());
    };
    same_canvas(&[(ref_path.to_owned(), ref_size), (path.to_owned(), size)]).map(|_| ())
}

/// The size of an image in the avatar to hold a new one to: the default
/// pose's base, else any other image that reads, never `except`.
fn canvas_reference(
    config: Option<&AvatarConfig>,
    files: Option<&BTreeMap<String, PathBuf>>,
    except: &str,
) -> Option<(String, UVec2)> {
    let (config, files) = (config?, files?);
    let sprite = config.sprite.as_ref()?;
    let default_base = sprite
        .poses
        .get(&sprite.default_pose)
        .map(|p| p.base.as_str());
    let paths = default_base
        .into_iter()
        .chain(config.paths().into_iter().filter(|p| {
            let p = p.to_ascii_lowercase();
            p.ends_with(".png") || p.ends_with(".webp")
        }));
    for path in paths {
        if path == except {
            continue;
        }
        if let Some(Ok(size)) = files.get(path).map(|file| file_size(file)) {
            return Some((path.to_owned(), size));
        }
    }
    None
}

/// What a pending file pick is for.
struct PickTarget {
    pose: String,
    slot: Slot,
}

/// The 2D section: the poses and the slots to put images in.
#[derive(Default)]
pub(crate) struct SpritePanel {
    pick: Option<(PickTicket, PickTarget)>,
    op: Option<OpTicket>,
    new_pose: String,
    /// The pose being renamed and the name typed so far.
    renaming: Option<(String, String)>,
    /// What this panel refused itself; the page shows the ops' errors.
    error: Option<String>,
    /// The images the renderer left out, as it last loaded them.
    left_out: LeftOut,
}

impl SpritePanel {
    pub(crate) fn new(left_out: LeftOut) -> Self {
        Self {
            left_out,
            ..Self::default()
        }
    }

    fn poll(&mut self, cx: &PanelContext<'_>) {
        if let Some(ticket) = &self.op
            && cx.ops.result(ticket).is_some()
        {
            self.op = None;
        }
        let Some((ticket, _)) = &self.pick else {
            return;
        };
        let Some(outcome) = cx.file_picker.result(ticket) else {
            return;
        };
        let Some((_, target)) = self.pick.take() else {
            return;
        };
        match outcome {
            Ok(None) => {}
            Err(e) => self.error = Some(e),
            Ok(Some(file)) => {
                if let Err(e) = self.upload(cx, target, file.bytes) {
                    self.error = Some(e);
                }
            }
        }
    }

    fn upload(
        &mut self,
        cx: &PanelContext<'_>,
        target: PickTarget,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let (size, ext) = image_info(&bytes)?;
        let path = format!("sprite/{}/{}.{ext}", target.pose, target.slot.key());
        let reference = canvas_reference(cx.config, cx.files, &path);
        check_canvas(
            reference.as_ref().map(|(p, s)| (p.as_str(), *s)),
            &path,
            size,
        )?;
        let PickTarget { pose, slot } = target;
        let stored = path.clone();
        self.op = Some(cx.ops.apply(SOURCE, vec![(path, bytes)], move |config| {
            // `apply`'s edit cannot fail, so a refusal (the pose is gone on
            // the agent's newer config) leaves the config unchanged and the
            // op still reports success. Only the log tells.
            if let Err(why) = with_image(config, &pose, slot, stored) {
                tracing::warn!("2D: image for pose {pose} not set: {why}");
            }
        }));
        Ok(())
    }

    fn choose(&mut self, cx: &PanelContext<'_>, pose: &str, slot: Slot) {
        self.error = None;
        let ticket = cx
            .file_picker
            .pick(format!("{} image for {pose}", slot.key()), vec![IMAGES]);
        self.pick = Some((
            ticket,
            PickTarget {
                pose: pose.to_owned(),
                slot,
            },
        ));
    }

    /// Runs a config-only edit after trying it on a copy, so a refusal shows
    /// here instead of being lost on the agent's side.
    fn edit(
        &mut self,
        cx: &PanelContext<'_>,
        edit: impl Fn(&mut AvatarConfig) -> Result<(), String> + Send + 'static,
    ) {
        self.error = None;
        if let Some(config) = cx.config
            && let Err(e) = edit(&mut config.clone())
        {
            self.error = Some(e);
            return;
        }
        self.op = Some(cx.ops.apply(SOURCE, Vec::new(), move |config| {
            // Refused on the agent's newer config: nothing changes, and
            // `apply` (whose edit cannot fail) still reports success. Only
            // the log tells.
            if let Err(why) = edit(config) {
                tracing::warn!("2D: pose edit not applied: {why}");
            }
        }));
    }
}

impl RendererPanel for SpritePanel {
    fn title(&self) -> &str {
        SOURCE
    }

    fn ui(&mut self, ui: &mut egui::Ui, cx: &mut PanelContext<'_>) {
        self.poll(cx);
        let picking = self.pick.is_some();
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(note) = left_out_note(&self.left_out.snapshot()) {
            ui.colored_label(ui.visuals().warn_fg_color, note);
        }
        let sprite = cx.config.and_then(|c| c.sprite.as_ref());
        match sprite {
            Some(sprite) if !sprite.poses.is_empty() => {
                for (name, pose) in &sprite.poses {
                    self.pose_ui(ui, cx, sprite, name, pose, picking);
                }
            }
            _ => {
                ui.weak("No poses yet. Add one with a base image to draw a 2D avatar.");
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("New pose");
            ui.add(egui::TextEdit::singleline(&mut self.new_pose).desired_width(120.0));
            let name = self.new_pose.trim().to_owned();
            let taken = sprite.is_some_and(|s| s.poses.contains_key(&name));
            let problem = name_error(&name)
                .map(str::to_owned)
                .or_else(|| taken.then(|| format!("there is already a pose named {name}")));
            let add = ui.add_enabled(
                !picking && problem.is_none(),
                egui::Button::new("Base image…"),
            );
            if add.clicked() {
                self.choose(cx, &name, Slot::Base);
                self.new_pose.clear();
            }
            if !name.is_empty()
                && let Some(problem) = problem
            {
                ui.weak(problem);
            }
        });
        if picking {
            ui.weak("Waiting for the file dialog…");
        }
    }
}

/// The note on the images the renderer left out, `None` when none were.
fn left_out_note(left_out: &BTreeMap<String, String>) -> Option<String> {
    if left_out.is_empty() {
        return None;
    }
    let each: Vec<String> = left_out
        .iter()
        .map(|(path, why)| format!("{path} ({why})"))
        .collect();
    Some(format!("Left out: {}", each.join("; ")))
}

impl SpritePanel {
    fn pose_ui(
        &mut self,
        ui: &mut egui::Ui,
        cx: &PanelContext<'_>,
        sprite: &SpriteConfig,
        name: &str,
        pose: &SpritePose,
        picking: bool,
    ) {
        let is_default = sprite.default_pose == name;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                if ui
                    .radio(is_default, "")
                    .on_hover_text("Default pose")
                    .clicked()
                    && !is_default
                {
                    let to = name.to_owned();
                    self.edit(cx, move |c| set_default(c, &to));
                }
                match &mut self.renaming {
                    Some((from, to)) if from == name => {
                        ui.add(egui::TextEdit::singleline(to).desired_width(120.0));
                        let to = to.trim().to_owned();
                        let from = from.clone();
                        if ui.button("Rename").clicked() {
                            self.renaming = None;
                            self.edit(cx, move |c| rename_pose(c, &from, &to));
                        } else if ui.button("Cancel").clicked() {
                            self.renaming = None;
                        }
                    }
                    _ => {
                        ui.strong(name);
                        if is_default {
                            ui.weak("(default)");
                        }
                        if ui.small_button("Rename").clicked() {
                            self.renaming = Some((name.to_owned(), name.to_owned()));
                        }
                        let delete = ui
                            .add_enabled(!is_default, egui::Button::new("Delete").small())
                            .on_disabled_hover_text(
                                "Choose another default pose before deleting this one",
                            );
                        if delete.clicked() {
                            let pose = name.to_owned();
                            self.edit(cx, move |c| remove_pose(c, &pose));
                        }
                    }
                }
            });
            egui::Grid::new(("sprite-pose", name)).show(ui, |ui| {
                let mut slot_row = |ui: &mut egui::Ui, slot: Slot, path: Option<&str>| {
                    ui.label(slot.key());
                    ui.weak(path.unwrap_or("none"));
                    if ui
                        .add_enabled(!picking, egui::Button::new("Set image…").small())
                        .clicked()
                    {
                        self.choose(cx, name, slot);
                    }
                    ui.end_row();
                };
                slot_row(ui, Slot::Base, Some(&pose.base));
                for key in SPRITE_EXPRESSIONS {
                    slot_row(
                        ui,
                        Slot::Expression(key),
                        pose.expressions.get(key).map(String::as_str),
                    );
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only_vrm() -> AvatarConfig {
        AvatarConfig {
            vrm: Some(sapphire_agent_core::VrmConfig {
                model: "model.vrm".into(),
                ..Default::default()
            }),
            sprite: None,
        }
    }

    fn two_poses() -> AvatarConfig {
        let mut config = AvatarConfig::default();
        with_image(
            &mut config,
            "normal",
            Slot::Base,
            "sprite/normal/base.png".into(),
        )
        .unwrap();
        with_image(
            &mut config,
            "arms",
            Slot::Base,
            "sprite/arms/base.png".into(),
        )
        .unwrap();
        config
    }

    #[test]
    fn with_image_creates_the_sprite_section_and_keeps_vrm() {
        let mut config = only_vrm();
        let vrm = config.vrm.clone();
        with_image(
            &mut config,
            "normal",
            Slot::Base,
            "sprite/normal/base.png".into(),
        )
        .unwrap();
        let sprite = config.sprite.unwrap();
        assert_eq!(sprite.default_pose, "normal");
        assert_eq!(sprite.poses["normal"].base, "sprite/normal/base.png");
        assert_eq!(config.vrm, vrm);
    }

    #[test]
    fn with_image_sets_an_expression_and_keeps_the_default() {
        let mut config = two_poses();
        with_image(
            &mut config,
            "arms",
            Slot::Expression("happy"),
            "sprite/arms/happy.webp".into(),
        )
        .unwrap();
        let sprite = config.sprite.unwrap();
        assert_eq!(sprite.default_pose, "normal");
        assert_eq!(
            sprite.poses["arms"].expressions["happy"],
            "sprite/arms/happy.webp"
        );
    }

    #[test]
    fn an_expression_needs_its_pose() {
        let mut config = only_vrm();
        let before = config.clone();
        assert!(with_image(&mut config, "gone", Slot::Expression("sad"), "x.png".into()).is_err());
        let mut config = two_poses();
        let before_poses = config.clone();
        assert!(with_image(&mut config, "gone", Slot::Expression("sad"), "x.png".into()).is_err());
        assert_eq!(config, before_poses);
        assert!(before.sprite.is_none());
        // A base brings a removed pose back.
        with_image(&mut config, "gone", Slot::Base, "b.png".into()).unwrap();
        assert!(config.sprite.unwrap().poses.contains_key("gone"));
    }

    #[test]
    fn pose_names_follow_the_agents_path_rules() {
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a:b",
            "x.uploading",
            "a/b",
            "a\\b",
        ] {
            assert!(name_error(bad).is_some(), "{bad:?}");
        }
        for good in ["normal", "arms up", "日本語", "a.b"] {
            assert!(name_error(good).is_none(), "{good:?}");
        }
        let mut config = two_poses();
        assert!(rename_pose(&mut config, "normal", ".hidden").is_err());
    }

    #[test]
    fn the_left_out_note_names_each_image_and_why() {
        assert_eq!(left_out_note(&BTreeMap::new()), None);
        let left_out = BTreeMap::from([
            ("a.png".to_string(), "3×3, not 2×4".to_string()),
            ("b.png".to_string(), "did not load".to_string()),
        ]);
        assert_eq!(
            left_out_note(&left_out).unwrap(),
            "Left out: a.png (3×3, not 2×4); b.png (did not load)"
        );
    }

    #[test]
    fn rename_follows_the_default() {
        let mut config = two_poses();
        rename_pose(&mut config, "normal", "calm").unwrap();
        let sprite = config.sprite.unwrap();
        assert_eq!(sprite.default_pose, "calm");
        assert!(sprite.poses.contains_key("calm") && !sprite.poses.contains_key("normal"));
    }

    #[test]
    fn rename_refuses_a_taken_name() {
        let mut config = two_poses();
        let before = config.clone();
        assert!(rename_pose(&mut config, "normal", "arms").is_err());
        assert!(rename_pose(&mut config, "normal", "").is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn removing_the_default_pose_is_refused() {
        let mut config = two_poses();
        assert!(remove_pose(&mut config, "normal").is_err());
        assert!(config.sprite.as_ref().unwrap().poses.contains_key("normal"));
        set_default(&mut config, "arms").unwrap();
        remove_pose(&mut config, "normal").unwrap();
        assert!(!config.sprite.unwrap().poses.contains_key("normal"));
    }

    #[test]
    fn a_wrong_sized_image_is_refused_before_upload() {
        let reference = Some(("sprite/normal/base.png", UVec2::new(64, 128)));
        let err =
            check_canvas(reference, "sprite/normal/happy.png", UVec2::new(32, 32)).unwrap_err();
        assert!(err.contains("32×32") && err.contains("64×128"), "{err}");
        assert_eq!(
            check_canvas(reference, "x.png", UVec2::new(64, 128)),
            Ok(())
        );
        // The first base sets the canvas.
        assert_eq!(check_canvas(None, "x.png", UVec2::new(7, 9)), Ok(()));
    }
}
