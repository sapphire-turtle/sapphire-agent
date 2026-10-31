//! Drawing the 2D avatar: its camera, the sprites of what [`shown`] picks,
//! the crossfade between them, and the bounds they cover.
//!
//! Two layers: `Current`, what [`shown`] picks now, and while a crossfade
//! runs `Previous`, what it picked before, drawn below and fading out as
//! `Current` fades in. Each layer is a base sprite and an optional overlay.
//!
//! Only a ready [`SpriteImages`] is drawn. When the avatar reloads (a new
//! generation) or fails, the sprites go at once rather than staying until
//! the new images are ready, so two generations never show together.

use std::time::Duration;

use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};

use super::load::{SpriteImages, sprite_config};
use super::resolve::shown;
use crate::avatar::{ActiveRenderer, AvatarAssets, AvatarBounds, AvatarState, RendererError};

/// How long a change of expression or pose crossfades.
pub(crate) const FADE: Duration = Duration::from_millis(200);

/// The sprite renderer's scene camera, below the UI camera.
#[derive(Component)]
pub(crate) struct SpriteCamera;

/// Which layer a sprite belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Current,
    Previous,
}

/// One sprite of the avatar: a layer's base or its overlay.
#[derive(Component)]
pub(crate) struct SpritePart {
    layer: Layer,
    overlay: bool,
}

impl SpritePart {
    /// Previous below current, each overlay above its base.
    fn z(&self) -> f32 {
        let layer = match self.layer {
            Layer::Previous => 0.0,
            Layer::Current => 2.0,
        };
        layer + if self.overlay { 1.0 } else { 0.0 }
    }
}

/// What the sprites show, and the crossfade under way.
#[derive(Resource, Default)]
pub(crate) struct SpriteScene {
    /// The current layer's base and overlay, as avatar paths.
    shown: Option<(String, Option<String>)>,
    fade: Option<Fade>,
}

#[derive(Clone, Copy)]
struct Fade {
    /// On the wall clock (`Time<Real>`), as the state's revert is.
    started: Duration,
    /// The previous layer's opacity when it started fading out: below one
    /// when it was itself still fading in.
    from: f32,
}

/// Where the canvas is drawn in a `window` of logical size: as tall as the
/// window, its aspect kept, centred horizontally and standing on the bottom
/// edge. Logical pixels, origin at the top left.
pub(crate) fn fit(canvas: UVec2, window: Vec2) -> Rect {
    let scale = window.y / canvas.y as f32;
    let width = canvas.x as f32 * scale;
    let left = (window.x - width) / 2.0;
    Rect::new(left, 0.0, left + width, window.y)
}

/// The camera and the sprites: what the renderer spawned.
type InScene = Or<(With<SpriteCamera>, With<SpritePart>)>;

/// Tears the scene down when the sprite renderer stops being chosen, and
/// forgets the loaded images so that coming back reloads and declares
/// again, even for the same generation. Ungated: it runs when the choice
/// changes, whichever renderer it went to.
pub(crate) fn despawn_sprite_scene(
    active: Res<ActiveRenderer>,
    entities: Query<Entity, InScene>,
    mut images: ResMut<SpriteImages>,
    mut scene: ResMut<SpriteScene>,
    mut commands: Commands,
) {
    if *active == ActiveRenderer::Sprite {
        return;
    }
    for entity in &entities {
        commands.entity(entity).despawn();
    }
    *images = SpriteImages::default();
    *scene = SpriteScene::default();
}

/// Spawns the sprites of what [`shown`] picks when the state or the images
/// change, starting a crossfade from what showed before. Clears them while
/// the images are not ready.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_sprites(
    assets: Res<AvatarAssets>,
    images: Res<SpriteImages>,
    state: Res<AvatarState>,
    error: Res<RendererError>,
    time: Res<Time<Real>>,
    mut scene: ResMut<SpriteScene>,
    mut parts: Query<(Entity, &mut SpritePart, &Sprite)>,
    cameras: Query<(), With<SpriteCamera>>,
    mut commands: Commands,
) {
    if !images.ready || error.0.is_some() {
        if scene.shown.is_some() || !parts.is_empty() {
            for (entity, ..) in &parts {
                commands.entity(entity).despawn();
            }
            *scene = SpriteScene::default();
        }
        return;
    }
    if scene.shown.is_some() && !state.is_changed() && !images.is_changed() {
        return;
    }
    let Some(config) = sprite_config(&assets) else {
        return;
    };
    let loaded = |path: &str| images.handles.contains_key(path) && !images.failed.contains(path);
    let Some(picked) = shown(config, &loaded, state.pose.as_deref(), &state.expression) else {
        return;
    };
    let next = (picked.base.to_owned(), picked.overlay.map(str::to_owned));
    if scene.shown.as_ref() == Some(&next) {
        return;
    }

    if cameras.is_empty() {
        commands.spawn((
            Camera2d,
            Camera {
                // Below the UI camera's `UI_CAMERA_ORDER`, which draws egui over it.
                order: 0,
                clear_color: ClearColorConfig::Custom(Color::NONE),
                ..default()
            },
            SpriteCamera,
        ));
    }

    // The first picture appears at once; a later one fades in over it.
    let fades = scene.shown.is_some();
    if fades {
        let mut from = 1.0;
        for (entity, mut part, sprite) in &mut parts {
            match part.layer {
                Layer::Previous => commands.entity(entity).despawn(),
                Layer::Current => {
                    from = sprite.color.alpha();
                    part.layer = Layer::Previous;
                }
            }
        }
        scene.fade = Some(Fade {
            started: time.elapsed(),
            from,
        });
    }
    let alpha = if fades { 0.0 } else { 1.0 };
    let layers = std::iter::once((&next.0, false)).chain(next.1.as_ref().map(|o| (o, true)));
    for (path, overlay) in layers {
        commands.spawn((
            Sprite {
                image: images.handles[path].clone(),
                color: Color::WHITE.with_alpha(alpha),
                ..default()
            },
            SpritePart {
                layer: Layer::Current,
                overlay,
            },
        ));
    }
    scene.shown = Some(next);
}

/// Runs the crossfade: current fades in, previous fades out, and is
/// despawned at the end. Asks for the next frame until it ends.
pub(crate) fn fade_sprites(
    time: Res<Time<Real>>,
    mut scene: ResMut<SpriteScene>,
    mut parts: Query<(Entity, &SpritePart, &mut Sprite)>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut commands: Commands,
) {
    let Some(fade) = scene.fade else {
        return;
    };
    let t =
        (time.elapsed().saturating_sub(fade.started).as_secs_f32() / FADE.as_secs_f32()).min(1.0);
    for (entity, part, mut sprite) in &mut parts {
        match part.layer {
            Layer::Current => sprite.color.set_alpha(t),
            Layer::Previous if t >= 1.0 => commands.entity(entity).despawn(),
            Layer::Previous => sprite.color.set_alpha(fade.from * (1.0 - t)),
        }
    }
    if t >= 1.0 {
        scene.fade = None;
    } else {
        redraw.write(RequestRedraw);
    }
}

/// Sizes and places the sprites on [`fit`]'s rect, following the window.
/// The camera's default projection puts world (0, 0) at the window's
/// centre, y up, one world unit to a logical pixel.
pub(crate) fn place_sprites(
    images: Res<SpriteImages>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut parts: Query<(&SpritePart, &mut Sprite, &mut Transform)>,
) {
    let (Some(canvas), Ok(window)) = (images.canvas, windows.single()) else {
        return;
    };
    let rect = fit(canvas, window.size());
    let size = rect.size();
    let centre = Vec2::new(
        rect.center().x - window.width() / 2.0,
        window.height() / 2.0 - rect.center().y,
    );
    for (part, mut sprite, mut transform) in &mut parts {
        if sprite.custom_size != Some(size) {
            sprite.custom_size = Some(size);
        }
        let translation = centre.extend(part.z());
        if transform.translation != translation {
            transform.translation = translation;
        }
    }
}

/// Reports the drawn rect as [`AvatarBounds`], `None` while nothing is
/// drawn. Leaves them to the placeholder while a [`RendererError`] is set.
pub(crate) fn write_bounds(
    images: Res<SpriteImages>,
    error: Res<RendererError>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut bounds: ResMut<AvatarBounds>,
) {
    if error.0.is_some() {
        return;
    }
    let next = match (images.ready, images.canvas, windows.single()) {
        (true, Some(canvas), Ok(window)) => Some(fit(canvas, window.size())),
        _ => None,
    };
    if bounds.0 != next {
        bounds.0 = next;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy::time::TimeUpdateStrategy;
    use bevy::window::{PrimaryWindow, RequestRedraw, WindowResolution};

    use super::*;
    use crate::avatar::sprite::load::tests::{app, cached, declared, png, settled, sprite, until};
    use crate::avatar::{
        ActiveRenderer, AvatarBounds, AvatarState, DeclaredAvatar, UI_CAMERA_ORDER,
    };

    #[test]
    fn fit_fills_the_height_and_centres() {
        let rect = fit(UVec2::new(400, 800), Vec2::new(420.0, 640.0));
        assert_eq!(rect, Rect::new(50.0, 0.0, 370.0, 640.0));
    }

    #[test]
    fn fit_follows_a_resize() {
        let rect = fit(UVec2::new(400, 800), Vec2::new(420.0, 320.0));
        assert_eq!(rect, Rect::new(130.0, 0.0, 290.0, 320.0));
    }

    /// A settled sprite avatar with a happy overlay, in a 420×640 window,
    /// whose clock advances 50 ms a frame.
    fn drawn(dir: &std::path::Path) -> App {
        let config = sprite(&[
            ("normal", "n.png", &[("happy", "nh.png")]),
            ("arms", "a.png", &[]),
        ]);
        let files = ["n.png", "nh.png", "a.png"].map(|p| (p, Some(png(2, 4))));
        let mut app = app(dir, cached(dir, config, &files));
        app.world_mut().spawn((
            Window {
                resolution: WindowResolution::new(420, 640),
                ..default()
            },
            PrimaryWindow,
        ));
        until(&mut app, settled);
        app.update();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            50,
        )));
        app
    }

    /// The sprite entities of each layer: (current, previous).
    fn layers(app: &mut App) -> (usize, usize) {
        let mut parts = app.world_mut().query::<&SpritePart>();
        let parts: Vec<_> = parts.iter(app.world()).map(|p| p.layer).collect();
        let count = |layer| parts.iter().filter(|l| **l == layer).count();
        (count(Layer::Current), count(Layer::Previous))
    }

    fn cameras(app: &mut App) -> usize {
        let mut cameras = app.world_mut().query::<&SpriteCamera>();
        cameras.iter(app.world()).count()
    }

    fn redraws(app: &mut App) -> usize {
        let mut cursor = app
            .world()
            .resource::<Messages<RequestRedraw>>()
            .get_cursor_current();
        app.update();
        let messages = app.world().resource::<Messages<RequestRedraw>>();
        cursor.read(messages).count()
    }

    #[test]
    fn the_avatar_is_drawn_where_its_bounds_say() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        assert_eq!(cameras(&mut app), 1);
        let mut camera = app
            .world_mut()
            .query_filtered::<&Camera, With<SpriteCamera>>();
        let camera = camera.single(app.world()).unwrap();
        assert!(camera.order < UI_CAMERA_ORDER);
        assert!(matches!(
            camera.clear_color,
            ClearColorConfig::Custom(color) if color == Color::NONE
        ));
        assert_eq!(layers(&mut app), (1, 0));
        let bounds = app.world().resource::<AvatarBounds>().0;
        // A 2×4 canvas, 640 tall in a 420-wide window: 320 wide, centred.
        assert_eq!(bounds, Some(Rect::new(50.0, 0.0, 370.0, 640.0)));

        // World (0, 0) is the window's centre, y up: the rect's centre is
        // (210, 320) logical, so the sprite sits at the world origin.
        let mut sprites = app.world_mut().query::<(&Sprite, &Transform)>();
        let (sprite, transform) = sprites.single(app.world()).unwrap();
        assert_eq!(sprite.custom_size, Some(Vec2::new(320.0, 640.0)));
        assert_eq!(transform.translation.truncate(), Vec2::ZERO);

        // A still avatar asks for no frames.
        assert_eq!(redraws(&mut app), 0);
    }

    #[test]
    fn a_resize_moves_the_sprites_and_the_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        let mut windows = app.world_mut().query::<&mut Window>();
        let mut window = windows.single_mut(app.world_mut()).unwrap();
        window.resolution.set(420.0, 320.0);
        app.update();

        let bounds = app.world().resource::<AvatarBounds>().0;
        assert_eq!(bounds, Some(Rect::new(130.0, 0.0, 290.0, 320.0)));
        let mut sprites = app.world_mut().query::<(&Sprite, &Transform)>();
        let (sprite, transform) = sprites.single(app.world()).unwrap();
        assert_eq!(sprite.custom_size, Some(Vec2::new(160.0, 320.0)));
        assert_eq!(transform.translation.truncate(), Vec2::ZERO);
    }

    #[test]
    fn a_change_crossfades_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("happy", 1.0);
        // The frame of the change starts the fade: the base and its overlay
        // fade in over the old base. Each frame is 50 ms; at 200 ms it ends.
        for frame in 0..4 {
            assert_eq!(redraws(&mut app), 1, "frame {frame} asks for the next");
            assert_eq!(layers(&mut app), (2, 1), "frame {frame}");
        }
        app.update();
        assert_eq!(layers(&mut app), (2, 0));
        let mut sprites = app.world_mut().query::<&Sprite>();
        assert!(sprites.iter(app.world()).all(|s| s.color.alpha() == 1.0));
        // Still again, it asks for no more frames.
        assert_eq!(redraws(&mut app), 0);
    }

    #[test]
    fn previous_fades_out_below_current() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        app.world_mut().resource_mut::<AvatarState>().pose("arms");
        app.update();
        app.update();
        let mut parts = app
            .world_mut()
            .query::<(&SpritePart, &Sprite, &Transform)>();
        let (mut current, mut previous) = (None, None);
        for (part, sprite, transform) in parts.iter(app.world()) {
            let seen = (sprite.color.alpha(), transform.translation.z);
            match part.layer {
                Layer::Current => current = Some(seen),
                Layer::Previous => previous = Some(seen),
            }
        }
        let (current, previous) = (current.unwrap(), previous.unwrap());
        assert!(0.0 < current.0 && current.0 < 1.0, "{current:?}");
        assert!(0.0 < previous.0 && previous.0 < 1.0, "{previous:?}");
        assert!(previous.1 < current.1, "{previous:?} below {current:?}");
    }

    #[test]
    fn switching_away_despawns_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        assert_eq!(cameras(&mut app), 1);

        // As `choose_renderer` does on a switch.
        app.insert_resource(ActiveRenderer::Placeholder);
        app.world_mut().resource_mut::<DeclaredAvatar>().0 = None;
        app.world_mut().resource_mut::<AvatarBounds>().0 = None;
        app.update();
        assert_eq!(cameras(&mut app), 0);
        assert_eq!(layers(&mut app), (0, 0));

        // Back, with the same generation: it loads and declares again.
        app.insert_resource(ActiveRenderer::Sprite);
        until(&mut app, settled);
        app.update();
        assert!(declared(&app).is_some());
        assert_eq!(cameras(&mut app), 1);
        assert_eq!(layers(&mut app), (1, 0));
        assert!(app.world().resource::<AvatarBounds>().0.is_some());
    }
}
