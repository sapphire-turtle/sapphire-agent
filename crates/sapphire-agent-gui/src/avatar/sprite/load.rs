//! Loading the `[sprite]` images through bevy's `AssetServer`, and checking
//! that they share one canvas before the avatar declares what it can show.

use std::collections::{BTreeMap, BTreeSet};

use bevy::asset::LoadState;
use bevy::prelude::*;
use bevy::window::RequestRedraw;
use sapphire_agent_core::SpriteConfig;

use super::resolve::{declared as declared_caps, same_canvas};
use crate::avatar::{AvatarAssets, DeclaredAvatar, RendererError};

/// The images of the avatar's `[sprite]` section, keyed by avatar path.
#[derive(Resource, Default)]
pub(crate) struct SpriteImages {
    /// The [`AvatarAssets::generation`] these were loaded for.
    pub generation: Option<u64>,
    pub handles: BTreeMap<String, Handle<Image>>,
    /// Paths that did not load, or whose size differs from the canvas.
    pub failed: BTreeSet<String>,
    /// The default pose's base size, which every image shares.
    pub canvas: Option<UVec2>,
    /// Every image settled and the default pose's base loaded.
    pub ready: bool,
}

impl SpriteImages {
    /// Whether there is nothing left to settle for this generation: it is
    /// ready, or the default pose's base failed.
    fn settled(&self, config: &SpriteConfig) -> bool {
        self.ready
            || config
                .poses
                .get(&config.default_pose)
                .is_some_and(|p| self.failed.contains(&p.base))
    }
}

/// The `[sprite]` section of the avatar that synced, if any.
fn sprite_config(assets: &AvatarAssets) -> Option<&SpriteConfig> {
    assets.loaded.as_ref()?.config.sprite.as_ref()
}

/// Starts loading every image of the `[sprite]` section when the synced
/// avatar changed, dropping the previous generation's handles.
pub(crate) fn load_sprites(
    assets: Res<AvatarAssets>,
    server: Res<AssetServer>,
    mut images: ResMut<SpriteImages>,
) {
    if images.generation == Some(assets.generation) {
        return;
    }
    let mut next = SpriteImages {
        generation: Some(assets.generation),
        ..default()
    };
    if let (Some(loaded), Some(config)) = (&assets.loaded, sprite_config(&assets)) {
        for pose in config.poses.values() {
            for path in std::iter::once(&pose.base).chain(pose.expressions.values()) {
                match loaded.asset_path(path) {
                    Some(at) => {
                        next.handles.insert(path.clone(), server.load(at));
                    }
                    None => {
                        next.failed.insert(path.clone());
                    }
                }
            }
        }
    }
    *images = next;
}

/// Once every image loaded or failed, checks the canvas and declares what
/// the avatar can show, or sets [`RendererError`] when the default pose's
/// base is unusable. Asks for frames while loads are pending: they complete
/// off-frame, and an idle desktop app would otherwise not wake for them.
pub(crate) fn settle_sprites(
    assets: Res<AvatarAssets>,
    server: Res<AssetServer>,
    textures: Res<Assets<Image>>,
    mut images: ResMut<SpriteImages>,
    mut declared: ResMut<DeclaredAvatar>,
    mut error: ResMut<RendererError>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Some(config) = sprite_config(&assets) else {
        return;
    };
    if images.generation != Some(assets.generation) || images.settled(config) {
        return;
    }
    let Some(default) = config.poses.get(&config.default_pose) else {
        fail(
            &mut error,
            &mut declared,
            format!(
                "the default pose {:?} is not among the sprite poses",
                config.default_pose
            ),
        );
        return;
    };

    // Each image's size, or why it is unusable.
    let mut outcomes: BTreeMap<String, Result<UVec2, String>> = BTreeMap::new();
    for (path, handle) in &images.handles {
        let outcome = match server.load_state(handle) {
            LoadState::Loaded => textures
                .get(handle)
                .map(Image::size)
                .ok_or_else(|| "it is not an image".to_owned()),
            LoadState::Failed(e) => Err(e.to_string()),
            LoadState::NotLoaded | LoadState::Loading => {
                redraw.write(RequestRedraw);
                return;
            }
        };
        outcomes.insert(path.clone(), outcome);
    }

    let canvas = match outcomes.get(default.base.as_str()) {
        Some(Ok(size)) => *size,
        Some(Err(cause)) => {
            let message = format!("{} did not load: {cause}", default.base);
            images.failed.insert(default.base.clone());
            fail(&mut error, &mut declared, message);
            return;
        }
        None => {
            let message = format!("{} is not among the synced files", default.base);
            // Already in `failed`, from `load_sprites`.
            fail(&mut error, &mut declared, message);
            return;
        }
    };

    let mut failed = std::mem::take(&mut images.failed);
    for (path, outcome) in outcomes {
        let cause = match outcome {
            Ok(size) => same_canvas(&[(default.base.clone(), canvas), (path.clone(), size)]).err(),
            Err(cause) => Some(format!("{path} did not load: {cause}")),
        };
        if let Some(cause) = cause {
            tracing::warn!("sprite image left out: {cause}");
            failed.insert(path);
        }
    }
    let caps = declared_caps(config, &|path: &str| {
        images.handles.contains_key(path) && !failed.contains(path)
    });
    images.failed = failed;
    images.canvas = Some(canvas);
    images.ready = true;

    if error.0.is_some() {
        error.0 = None;
    }
    if declared.0 != caps {
        declared.0 = caps;
    }
}

/// Reports why the avatar cannot be drawn, and declares nothing.
fn fail(error: &mut ResMut<RendererError>, declared: &mut ResMut<DeclaredAvatar>, message: String) {
    if error.0.as_ref() != Some(&message) {
        tracing::warn!("the 2D avatar cannot be drawn: {message}");
        error.0 = Some(message);
    }
    if declared.0.is_some() {
        declared.0 = None;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use bevy::asset::RenderAssetUsages;
    use bevy::asset::{AssetApp as _, AssetPlugin};
    use bevy::image::{CompressedImageFormats, ImageLoader, ImagePlugin};
    use bevy::prelude::*;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
    use bevy::window::RequestRedraw;
    use sapphire_agent_core::{AvatarConfig, SpriteConfig, SpritePose};

    use crate::avatar::assets::{Cache, sha256_hex};
    use crate::avatar::sprite::SpriteRendererPlugin;
    use crate::avatar::{
        ActiveRenderer, AvatarAssets, DeclaredAvatar, LoadedAvatar, RendererError,
        register_asset_source_at,
    };

    /// A `w`×`h` PNG, encoded through bevy's own image conversion.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let image = Image::new_fill(
            Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[255, 0, 0, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .try_into_dynamic()
            .unwrap()
            .write_to(
                &mut bytes,
                bevy::image::ImageFormat::Png
                    .as_image_crate_format()
                    .unwrap(),
            )
            .unwrap();
        bytes.into_inner()
    }

    /// A pose: its name, its base, and its `(expression, path)` overlays.
    type Pose<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// A sprite avatar with these poses; the first is the default.
    fn sprite(poses: &[Pose]) -> AvatarConfig {
        AvatarConfig {
            vrm: None,
            sprite: Some(SpriteConfig {
                default_pose: poses[0].0.into(),
                poses: poses
                    .iter()
                    .map(|(name, base, expressions)| {
                        let pose = SpritePose {
                            base: (*base).into(),
                            expressions: expressions
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect(),
                        };
                        (name.to_string(), pose)
                    })
                    .collect(),
            }),
        }
    }

    /// Caches `files` (avatar path, bytes) and describes the avatar holding
    /// them. A file with `None` bytes is referenced but absent from the cache.
    fn cached(
        dir: &std::path::Path,
        config: AvatarConfig,
        files: &[(&str, Option<Vec<u8>>)],
    ) -> LoadedAvatar {
        let cache = Cache::new(dir.to_path_buf());
        let files = files
            .iter()
            .map(|(path, bytes)| {
                let at = match bytes {
                    Some(bytes) => cache.put(&sha256_hex(bytes), path, bytes).unwrap(),
                    None => cache.path_of(&sha256_hex(path.as_bytes()), path).unwrap(),
                };
                (path.to_string(), at)
            })
            .collect::<BTreeMap<_, _>>();
        LoadedAvatar { config, files }
    }

    fn app(dir: &std::path::Path, avatar: LoadedAvatar) -> App {
        let mut app = App::new();
        register_asset_source_at(&mut app, dir.to_path_buf());
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            ImagePlugin::default(),
        ))
        // bevy's render plugin registers the image loader; there is none here.
        .register_asset_loader(ImageLoader::new(CompressedImageFormats::NONE))
        .insert_resource(ActiveRenderer::Sprite)
        .init_resource::<DeclaredAvatar>()
        .init_resource::<RendererError>()
        .init_resource::<AvatarAssets>()
        .add_plugins(SpriteRendererPlugin);
        app.world_mut()
            .resource_mut::<AvatarAssets>()
            .apply(Ok(avatar));
        app
    }

    /// Runs frames until `done`, or fails after a while.
    fn until(app: &mut App, done: impl Fn(&World) -> bool) {
        for _ in 0..500 {
            app.update();
            if done(app.world()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the sprites never settled");
    }

    fn declared(app: &App) -> Option<(Vec<String>, Vec<String>)> {
        let caps = app.world().resource::<DeclaredAvatar>().0.as_ref()?;
        Some((caps.poses.clone(), caps.expressions.clone()))
    }

    fn error(app: &App) -> Option<String> {
        app.world().resource::<RendererError>().0.clone()
    }

    fn settled(world: &World) -> bool {
        world.resource::<DeclaredAvatar>().0.is_some()
            || world.resource::<RendererError>().0.is_some()
    }

    #[test]
    fn a_complete_sprite_avatar_declares_its_cues() {
        let dir = tempfile::tempdir().unwrap();
        let config = sprite(&[
            ("normal", "n.png", &[("happy", "nh.png")]),
            ("arms", "a.png", &[("sad", "as.png")]),
        ]);
        let files = ["n.png", "nh.png", "a.png", "as.png"].map(|p| (p, Some(png(2, 4))));
        let mut app = app(dir.path(), cached(dir.path(), config, &files));

        until(&mut app, settled);
        assert_eq!(error(&app), None);
        assert_eq!(
            declared(&app),
            Some((
                vec!["arms".into(), "normal".into()],
                vec!["neutral".into(), "happy".into(), "sad".into()]
            ))
        );

        // Settled, it asks for no more frames.
        let mut after = app
            .world()
            .resource::<Messages<RequestRedraw>>()
            .get_cursor_current();
        app.update();
        app.update();
        let messages = app.world().resource::<Messages<RequestRedraw>>();
        assert_eq!(after.read(messages).count(), 0);
    }

    #[test]
    fn a_wrong_sized_overlay_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let config = sprite(&[("normal", "n.png", &[("happy", "nh.png"), ("sad", "ns.png")])]);
        let files = [
            ("n.png", Some(png(2, 4))),
            ("nh.png", Some(png(3, 3))),
            ("ns.png", Some(png(2, 4))),
        ];
        let mut app = app(dir.path(), cached(dir.path(), config, &files));

        until(&mut app, settled);
        assert_eq!(error(&app), None);
        assert_eq!(
            declared(&app),
            Some((vec!["normal".into()], vec!["neutral".into(), "sad".into()]))
        );
    }

    #[test]
    fn a_missing_default_base_is_a_renderer_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = sprite(&[
            ("normal", "n.png", &[("happy", "nh.png")]),
            ("arms", "a.png", &[]),
        ]);
        let files = [
            ("n.png", None),
            ("nh.png", Some(png(2, 4))),
            ("a.png", Some(png(2, 4))),
        ];
        let mut app = app(dir.path(), cached(dir.path(), config, &files));

        until(&mut app, settled);
        let error = error(&app).expect("a renderer error");
        assert!(error.contains("n.png"), "{error}");
        assert_eq!(declared(&app), None);
    }

    #[test]
    fn a_new_generation_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = sprite(&[("normal", "n.png", &[])]);
        let files = [("n.png", Some(png(2, 4)))];
        let mut app = app(dir.path(), cached(dir.path(), first, &files));
        until(&mut app, settled);
        assert_eq!(
            declared(&app),
            Some((vec!["normal".into()], vec!["neutral".into()]))
        );

        let second = sprite(&[
            ("normal", "n.png", &[]),
            ("arms", "a.png", &[("angry", "aa.png")]),
        ]);
        let files = ["n.png", "a.png", "aa.png"].map(|p| (p, Some(png(2, 4))));
        let avatar = cached(dir.path(), second, &files);
        app.world_mut()
            .resource_mut::<AvatarAssets>()
            .apply(Ok(avatar));
        until(&mut app, |world| {
            world
                .resource::<DeclaredAvatar>()
                .0
                .as_ref()
                .is_some_and(|caps| caps.poses.len() == 2)
        });
        assert_eq!(error(&app), None);
        assert_eq!(
            declared(&app),
            Some((
                vec!["arms".into(), "normal".into()],
                vec!["neutral".into(), "angry".into()]
            ))
        );
    }
}
