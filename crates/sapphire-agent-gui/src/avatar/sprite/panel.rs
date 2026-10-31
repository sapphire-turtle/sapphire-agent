//! The "2D" section of the Character page (spec §7): the sprite avatar's
//! poses, and the image in each pose's slots.
//!
//! The edits are pure functions over [`AvatarConfig`] so they test without
//! egui; the panel only wires them to buttons and to the file picker.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use bevy::math::UVec2;
use bevy_egui::egui;
use sapphire_agent_core::{AvatarConfig, SPRITE_EXPRESSIONS, SpriteConfig, SpritePose};

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

/// Points `slot` of `pose` at `path`, creating `[sprite]` (with `pose` as
/// the default) and the pose as needed. `vrm` is left alone.
pub(crate) fn with_image(config: &mut AvatarConfig, pose: &str, slot: Slot, path: String) {
    let sprite = config.sprite.get_or_insert_with(|| SpriteConfig {
        default_pose: pose.to_owned(),
        poses: BTreeMap::new(),
    });
    let pose = sprite.poses.entry(pose.to_owned()).or_default();
    match slot {
        Slot::Base => pose.base = path,
        Slot::Expression(key) => {
            pose.expressions.insert(key.to_owned(), path);
        }
    }
}

/// Why `name` cannot name a pose: it becomes a directory of the upload path.
fn name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("a pose needs a name")
    } else if name == "." || name == ".." || name.contains(['/', '\\']) {
        Some("a pose name cannot contain slashes or be . or ..")
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

/// The pixel size and file extension of a PNG or WebP, read from its
/// header only.
pub(crate) fn image_info(bytes: &[u8]) -> Result<(UVec2, &'static str), String> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let short = || "the image file is truncated".to_owned();
    let le24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    if bytes.starts_with(PNG) {
        let ihdr = bytes.get(8..24).ok_or_else(short)?;
        if &ihdr[4..8] != b"IHDR" {
            return Err("the PNG has no header".to_owned());
        }
        let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        return Ok((UVec2::new(be(&ihdr[8..12]), be(&ihdr[12..16])), "png"));
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let chunk = bytes.get(12..30).ok_or_else(short)?;
        let size = match &chunk[..4] {
            // Lossy: frame tag, start code, then 14-bit width and height.
            b"VP8 " if chunk[11..14] == [0x9d, 0x01, 0x2a] => UVec2::new(
                u32::from(u16::from_le_bytes([chunk[14], chunk[15]]) & 0x3fff),
                u32::from(u16::from_le_bytes([chunk[16], chunk[17]]) & 0x3fff),
            ),
            // Lossless: signature byte, then 14-bit width-1 and height-1.
            b"VP8L" if chunk[8] == 0x2f => {
                let bits = u32::from_le_bytes([chunk[9], chunk[10], chunk[11], chunk[12]]);
                UVec2::new((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            // Extended: 24-bit width-1 and height-1 after the flags.
            b"VP8X" => UVec2::new(le24(&chunk[12..15]) + 1, le24(&chunk[15..18]) + 1),
            _ => return Err("this WebP is not one the header can be read from".to_owned()),
        };
        return Ok((size, "webp"));
    }
    Err("the file is not a PNG or WebP image".to_owned())
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
    let paths = default_base.into_iter().chain(
        config
            .paths()
            .into_iter()
            .filter(|p| p.ends_with(".png") || p.ends_with(".webp")),
    );
    for path in paths {
        if path == except {
            continue;
        }
        let Some(file) = files.get(path) else {
            continue;
        };
        // Only the header: 32 bytes of a possibly large file.
        let mut head = Vec::new();
        let read = std::fs::File::open(file).and_then(|f| f.take(32).read_to_end(&mut head));
        if read.is_ok()
            && let Ok((size, _)) = image_info(&head)
        {
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
}

impl SpritePanel {
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
            with_image(config, &pose, slot, stored);
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
            // Refused on the agent's newer config: nothing changes.
            let _ = edit(config);
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
        );
        with_image(
            &mut config,
            "arms",
            Slot::Base,
            "sprite/arms/base.png".into(),
        );
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
        );
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
        );
        let sprite = config.sprite.unwrap();
        assert_eq!(sprite.default_pose, "normal");
        assert_eq!(
            sprite.poses["arms"].expressions["happy"],
            "sprite/arms/happy.webp"
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

    #[test]
    fn image_info_reads_png_and_webp_headers() {
        let png = crate::avatar::sprite::load::tests::png(3, 5);
        assert_eq!(image_info(&png), Ok((UVec2::new(3, 5), "png")));

        let riff = |chunk: &[u8]| {
            let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
            b.extend_from_slice(chunk);
            b.resize(40, 0);
            b
        };
        // VP8X: flags, then 24-bit width-1 and height-1.
        let vp8x = riff(&[
            b'V', b'P', b'8', b'X', 10, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 199, 0, 0,
        ]);
        assert_eq!(image_info(&vp8x), Ok((UVec2::new(100, 200), "webp")));
        // VP8L: signature 0x2f, then 14-bit width-1 and height-1.
        let packed: u32 = 49 | (79 << 14);
        let mut l = vec![b'V', b'P', b'8', b'L', 5, 0, 0, 0, 0x2f];
        l.extend_from_slice(&packed.to_le_bytes());
        assert_eq!(image_info(&riff(&l)), Ok((UVec2::new(50, 80), "webp")));
        // VP8: frame tag, start code, 14-bit width and height.
        let vp8 = riff(&[
            b'V', b'P', b'8', b' ', 10, 0, 0, 0, 0, 0, 0, 0x9d, 0x01, 0x2a, 30, 0, 40, 0,
        ]);
        assert_eq!(image_info(&vp8), Ok((UVec2::new(30, 40), "webp")));

        assert!(image_info(b"GIF89a....").is_err());
        assert!(image_info(&png[..10]).is_err());
    }
}
