//! Drawing the 2D avatar: its camera, the sprites of what [`shown`] picks,
//! the crossfade between them, and the bounds they cover.
//!
//! The `Current` sprites are what [`shown`] picks now: a base and an
//! optional overlay. A change fades without letting the desktop show
//! through the avatar:
//!
//! - **Expression only** (same base): the base stays, opaque. The old
//!   overlay fades out and the new one fades in above it.
//! - **Pose**: everything shown becomes `Previous`, drawn underneath and held
//!   at its opacity for the first half of the fade before fading out, while
//!   the new base and overlay fade in on top.
//!
//! Each sprite fades on its own from the opacity it has, so a change in the
//! middle of a fade carries on from what is on screen.
//!
//! Only a ready [`SpriteImages`] is drawn. When the avatar reloads (a new
//! generation, as every upload from the panel makes), what shows stays,
//! bounds included, until the new images are ready, then the new picture
//! replaces it in one frame, so two generations never show together. A
//! [`RendererError`] clears the sprites at once: the placeholder takes over.

use std::time::Duration;

use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};

use super::load::{LeftOut, SpriteImages, sprite_config};
use super::resolve::shown;
use crate::avatar::{ActiveRenderer, AvatarAssets, AvatarBounds, AvatarState, RendererError};

/// How long a change of expression or pose crossfades.
pub(crate) const FADE: Duration = Duration::from_millis(200);

/// The sprite renderer's scene camera, below the UI camera.
#[derive(Component)]
pub(crate) struct SpriteCamera;

/// Whether a sprite is part of what shows now, or on its way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Current,
    Previous,
}

/// The current base's depth; its overlay is drawn one above it.
const BASE_Z: f32 = 0.0;
/// An overlay fading out over the base that stays: above the base, below
/// the overlay fading in. Each later one goes [`OUTGOING_STEP_Z`] lower,
/// so overlapping fades draw in a fixed order.
const OUTGOING_OVERLAY_Z: f32 = BASE_Z + 0.5;
const OUTGOING_STEP_Z: f32 = 1.0 / 64.0;
/// How far a pose change pushes what showed below the new pose.
const POSE_STEP_Z: f32 = 10.0;

/// One sprite of the avatar: a base or an overlay.
#[derive(Component)]
pub(crate) struct SpritePart {
    layer: Layer,
    overlay: bool,
    z: f32,
    fade: Option<PartFade>,
}

/// How a sprite's opacity changes, from `from` at `started`.
#[derive(Clone, Copy)]
struct PartFade {
    /// On the wall clock (`Time<Real>`), as the state's revert is.
    started: Duration,
    from: f32,
    curve: Curve,
}

#[derive(Clone, Copy)]
enum Curve {
    /// Up to opaque.
    In,
    /// Down to nothing.
    Out,
    /// Held for the first half, then down to nothing: what the new pose
    /// fades in over stays covered until the new pose mostly is.
    HoldThenOut,
}

impl PartFade {
    fn new(started: Duration, from: f32, curve: Curve) -> Self {
        Self {
            started,
            from,
            curve,
        }
    }

    /// The share of [`FADE`] gone by at `now`, up to one.
    fn progress(&self, now: Duration) -> f32 {
        (now.saturating_sub(self.started).as_secs_f32() / FADE.as_secs_f32()).min(1.0)
    }

    fn alpha(&self, t: f32) -> f32 {
        match self.curve {
            Curve::In => self.from + (1.0 - self.from) * t,
            Curve::Out => self.from * (1.0 - t),
            Curve::HoldThenOut => self.from * (2.0 * (1.0 - t)).min(1.0),
        }
    }
}

/// What the current sprites show.
#[derive(Resource, Default)]
pub(crate) struct SpriteScene {
    /// The current base and overlay, as avatar paths.
    shown: Option<(String, Option<String>)>,
    /// The [`SpriteImages::generation`] the sprites were drawn from.
    generation: Option<u64>,
    /// Their canvas size, which places them and makes the bounds; kept
    /// while the next generation loads.
    canvas: Option<UVec2>,
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
    left_out: Res<LeftOut>,
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
    left_out.set(Default::default());
}

/// Spawns the sprites of what [`shown`] picks when the state or the images
/// change, and starts their crossfade from what showed before (see the
/// module docs). Keeps them while a new generation loads; clears them on a
/// [`RendererError`].
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
    let config = sprite_config(&assets).filter(|_| error.0.is_none());
    let Some(config) = config else {
        if scene.shown.is_some() || !parts.is_empty() {
            for (entity, ..) in &parts {
                commands.entity(entity).despawn();
            }
            *scene = SpriteScene::default();
        }
        return;
    };
    // Loading: what shows stays until the new images are ready.
    if !images.ready {
        return;
    }
    let reloaded = scene.generation != images.generation;
    if !reloaded && scene.shown.is_some() && !state.is_changed() {
        return;
    }
    let loaded =
        |path: &str| images.handles.contains_key(path) && !images.failed.contains_key(path);
    let Some(picked) = shown(config, &loaded, state.pose.as_deref(), &state.expression) else {
        return;
    };
    let next = (picked.base.to_owned(), picked.overlay.map(str::to_owned));
    if !reloaded && scene.shown.as_ref() == Some(&next) {
        return;
    }
    if reloaded {
        // The new generation replaces the old in one frame.
        for (entity, ..) in &parts {
            commands.entity(entity).despawn();
        }
        *scene = SpriteScene {
            shown: None,
            generation: images.generation,
            canvas: images.canvas,
        };
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

    let now = time.elapsed();
    let mut spawn = |path: &str, overlay: bool, fade: Option<PartFade>| {
        let z = BASE_Z + if overlay { 1.0 } else { 0.0 };
        let alpha = fade.map_or(1.0, |f| f.alpha(0.0));
        commands.spawn((
            Sprite {
                image: images.handles[path].clone(),
                color: Color::WHITE.with_alpha(alpha),
                ..default()
            },
            SpritePart {
                layer: Layer::Current,
                overlay,
                z,
                fade,
            },
        ));
    };
    let fade_in = Some(PartFade::new(now, 0.0, Curve::In));
    match &scene.shown {
        // The first picture, or a reloaded one, appears at once.
        None => {
            spawn(&next.0, false, None);
            if let Some(overlay) = &next.1 {
                spawn(overlay, true, None);
            }
        }
        // The base stays: only the overlays change.
        Some((base, _)) if *base == next.0 => {
            // Below the overlays already fading out over this base.
            let z = parts
                .iter()
                .filter(|(_, p, _)| p.layer == Layer::Previous && p.overlay && p.z > BASE_Z)
                .map(|(_, p, _)| p.z - OUTGOING_STEP_Z)
                .fold(OUTGOING_OVERLAY_Z, f32::min)
                .max(BASE_Z + OUTGOING_STEP_Z);
            for (_, mut part, sprite) in &mut parts {
                if part.layer == Layer::Current && part.overlay {
                    part.layer = Layer::Previous;
                    part.z = z;
                    part.fade = Some(PartFade::new(now, sprite.color.alpha(), Curve::Out));
                }
            }
            if let Some(overlay) = &next.1 {
                spawn(overlay, true, fade_in);
            }
        }
        // A new pose fades in over everything that showed.
        Some(_) => {
            for (_, mut part, sprite) in &mut parts {
                part.layer = Layer::Previous;
                part.z -= POSE_STEP_Z;
                part.fade = Some(PartFade::new(now, sprite.color.alpha(), Curve::HoldThenOut));
            }
            spawn(&next.0, false, fade_in);
            if let Some(overlay) = &next.1 {
                spawn(overlay, true, fade_in);
            }
        }
    }
    scene.shown = Some(next);
}

/// Runs the crossfades: sets each fading sprite's opacity, despawns what
/// faded out, and asks for the next frame until every fade ends.
pub(crate) fn fade_sprites(
    time: Res<Time<Real>>,
    mut parts: Query<(Entity, &mut SpritePart, &mut Sprite)>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let mut fading = false;
    for (entity, mut part, mut sprite) in &mut parts {
        let Some(fade) = part.fade else {
            continue;
        };
        let t = fade.progress(now);
        if t < 1.0 {
            sprite.color.set_alpha(fade.alpha(t));
            fading = true;
        } else if part.layer == Layer::Previous {
            commands.entity(entity).despawn();
        } else {
            sprite.color.set_alpha(1.0);
            part.fade = None;
        }
    }
    if fading {
        redraw.write(RequestRedraw);
    }
}

/// Sizes and places the sprites on [`fit`]'s rect, following the window.
/// The camera's default projection puts world (0, 0) at the window's
/// centre, y up, one world unit to a logical pixel.
pub(crate) fn place_sprites(
    scene: Res<SpriteScene>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut parts: Query<(&SpritePart, &mut Sprite, &mut Transform)>,
) {
    let (Some(canvas), Ok(window)) = (scene.canvas, windows.single()) else {
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
        let translation = centre.extend(part.z);
        if transform.translation != translation {
            transform.translation = translation;
        }
    }
}

/// Reports the drawn rect as [`AvatarBounds`], `None` while nothing is
/// drawn. Leaves them to the placeholder while a [`RendererError`] is set.
pub(crate) fn write_bounds(
    scene: Res<SpriteScene>,
    error: Res<RendererError>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut bounds: ResMut<AvatarBounds>,
) {
    if error.0.is_some() {
        return;
    }
    let next = match (scene.canvas, windows.single()) {
        (Some(canvas), Ok(window)) => Some(fit(canvas, window.size())),
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
        ActiveRenderer, AvatarAssets, AvatarBounds, AvatarState, DeclaredAvatar, RendererError,
        UI_CAMERA_ORDER,
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

    /// A settled sprite avatar with happy and sad overlays and a second pose, in a 420×640 window,
    /// whose clock advances 50 ms a frame.
    fn drawn(dir: &std::path::Path) -> App {
        let config = sprite(&[
            ("normal", "n.png", &[("happy", "nh.png"), ("sad", "ns.png")]),
            ("arms", "a.png", &[]),
        ]);
        let files = ["n.png", "nh.png", "ns.png", "a.png"].map(|p| (p, Some(png(2, 4))));
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

    /// Every sprite: its entity, layer, whether it is an overlay, its
    /// opacity and its depth.
    fn parts(app: &mut App) -> Vec<(Entity, Layer, bool, f32, f32)> {
        let mut parts = app
            .world_mut()
            .query::<(Entity, &SpritePart, &Sprite, &Transform)>();
        parts
            .iter(app.world())
            .map(|(e, p, s, t)| (e, p.layer, p.overlay, s.color.alpha(), t.translation.z))
            .collect()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// The one current base: its entity and opacity.
    fn base(app: &mut App) -> (Entity, f32) {
        let bases: Vec<_> = parts(app)
            .into_iter()
            .filter(|(_, layer, overlay, ..)| *layer == Layer::Current && !overlay)
            .map(|(e, _, _, alpha, _)| (e, alpha))
            .collect();
        assert_eq!(bases.len(), 1, "{bases:?}");
        bases[0]
    }

    #[test]
    fn an_expression_change_fades_only_the_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        let (still, _) = base(&mut app);

        // neutral → happy: the one overlay fades in over the base, which
        // stays as it is. Each frame is 50 ms; at 200 ms it ends.
        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("happy", 1.0);
        for frame in 0..4 {
            assert_eq!(redraws(&mut app), 1, "frame {frame} asks for the next");
            assert_eq!(layers(&mut app), (2, 0), "frame {frame}");
            assert_eq!(base(&mut app), (still, 1.0), "frame {frame}");
        }
        app.update();
        assert_eq!(layers(&mut app), (2, 0));
        assert!(parts(&mut app).iter().all(|p| p.3 == 1.0));
        assert_eq!(redraws(&mut app), 0, "still again, it asks for no frames");

        // happy → sad: the old overlay fades out between the base and the
        // new overlay, the base untouched.
        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("sad", 1.0);
        for frame in 0..4 {
            app.update();
            assert_eq!(layers(&mut app), (2, 1), "frame {frame}");
            assert_eq!(base(&mut app), (still, 1.0), "frame {frame}");
            let parts = parts(&mut app);
            let z = |layer, overlay| {
                parts
                    .iter()
                    .find(|p| p.1 == layer && p.2 == overlay)
                    .unwrap()
                    .4
            };
            let base_z = z(Layer::Current, false);
            let (outgoing, incoming) = (z(Layer::Previous, true), z(Layer::Current, true));
            assert!(base_z < outgoing && outgoing < incoming, "{parts:?}");
        }
        app.update();
        assert_eq!(layers(&mut app), (2, 0));
        assert_eq!(base(&mut app), (still, 1.0));
        assert_eq!(redraws(&mut app), 0);
    }

    #[test]
    fn a_pose_change_holds_the_old_pose_underneath() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        app.world_mut().resource_mut::<AvatarState>().pose("arms");
        app.update(); // t = 0
        app.update(); // t = 0.25
        app.update(); // t = 0.5
        let alphas = |app: &mut App| {
            let parts = parts(app);
            assert_eq!(parts.len(), 2, "{parts:?}");
            let of = |layer| parts.iter().find(|p| p.1 == layer).unwrap();
            let (previous, current) = (of(Layer::Previous), of(Layer::Current));
            assert!(previous.4 < current.4, "previous is drawn below: {parts:?}");
            (previous.3, current.3)
        };
        let (previous, current) = alphas(&mut app);
        assert!(close(previous, 1.0), "held at `from` halfway: {previous}");
        assert!(close(current, 0.5), "{current}");

        app.update(); // t = 0.75
        let (previous, current) = alphas(&mut app);
        assert!(close(previous, 0.5), "{previous}");
        assert!(close(current, 0.75), "{current}");

        app.update(); // t = 1
        assert_eq!(layers(&mut app), (1, 0));
        assert_eq!(base(&mut app).1, 1.0);
        assert_eq!(redraws(&mut app), 0);
    }

    #[test]
    fn a_change_mid_fade_carries_the_opacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());

        app.world_mut().resource_mut::<AvatarState>().pose("arms");
        app.update(); // t = 0
        app.update(); // t = 0.25: arms a quarter in, normal held opaque
        let before = parts(&mut app);

        app.world_mut().resource_mut::<AvatarState>().pose("normal");
        app.update();
        let after = parts(&mut app);
        for (entity, _, _, alpha, _) in &before {
            let (.., now, _) = after.iter().find(|p| p.0 == *entity).unwrap();
            assert!(close(*now, *alpha), "{alpha} became {now}");
            assert!(
                after
                    .iter()
                    .any(|p| p.0 == *entity && p.1 == Layer::Previous)
            );
        }
        assert_eq!(layers(&mut app), (1, 2));

        // Everything ends with the one base, opaque.
        for _ in 0..4 {
            app.update();
        }
        assert_eq!(layers(&mut app), (1, 0));
        assert_eq!(base(&mut app).1, 1.0);
    }

    #[test]
    fn overlapping_outgoing_overlays_draw_in_a_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("happy", 1.0);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(layers(&mut app), (2, 0));

        // happy → sad, and back before the fade ends: two overlays fade out
        // over the base at once.
        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("sad", 1.0);
        app.update();
        app.world_mut()
            .resource_mut::<AvatarState>()
            .expression("happy", 1.0);
        app.update();
        assert_eq!(layers(&mut app), (2, 2));

        // Told apart by when each began to fade: the files share their
        // bytes, so their handles are the same.
        let mut query = app.world_mut().query::<&SpritePart>();
        let mut outgoing: Vec<_> = query
            .iter(app.world())
            .filter(|p| p.layer == Layer::Previous && p.overlay)
            .map(|p| (p.fade.unwrap().started, p.z))
            .collect();
        outgoing.sort_by_key(|(started, _)| *started);
        let (first_out, then_out) = (outgoing[0].1, outgoing[1].1);
        let incoming = query
            .iter(app.world())
            .find(|p| p.layer == Layer::Current && p.overlay)
            .unwrap()
            .z;
        assert!(
            BASE_Z < then_out && then_out < first_out && first_out < incoming,
            "{then_out} {first_out} {incoming}"
        );
    }

    #[test]
    fn a_reload_keeps_the_old_scene_until_the_new_one_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        let old_parts: Vec<Entity> = parts(&mut app).iter().map(|p| p.0).collect();
        let old_bounds = app.world().resource::<AvatarBounds>().0;
        assert!(old_bounds.is_some());

        // New art, of another size: what an upload from the panel makes.
        let config = sprite(&[("normal", "n2.png", &[])]);
        let avatar = cached(dir.path(), config, &[("n2.png", Some(png(4, 4)))]);
        app.world_mut()
            .resource_mut::<AvatarAssets>()
            .apply(Ok(avatar));
        app.update();
        assert!(
            !app.world().resource::<SpriteImages>().ready,
            "still loading"
        );
        let now: Vec<Entity> = parts(&mut app).iter().map(|p| p.0).collect();
        assert_eq!(now, old_parts, "the old picture stays while loading");
        assert_eq!(app.world().resource::<AvatarBounds>().0, old_bounds);

        until(&mut app, |world| world.resource::<SpriteImages>().ready);
        app.update();
        // Swapped in one frame: one opaque base, none of the old sprites.
        assert_eq!(layers(&mut app), (1, 0));
        let (entity, alpha) = base(&mut app);
        assert!(!old_parts.contains(&entity));
        assert_eq!(alpha, 1.0);
        assert_eq!(
            app.world().resource::<AvatarBounds>().0,
            Some(fit(UVec2::new(4, 4), Vec2::new(420.0, 640.0)))
        );
        assert_eq!(redraws(&mut app), 0);
    }

    #[test]
    fn a_failed_reload_clears_the_sprites_and_leaves_the_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        assert_eq!(layers(&mut app), (1, 0));

        // A new generation whose default base is missing.
        let config = sprite(&[("normal", "gone.png", &[])]);
        let avatar = cached(dir.path(), config, &[("gone.png", None)]);
        app.world_mut()
            .resource_mut::<AvatarAssets>()
            .apply(Ok(avatar));
        until(&mut app, |world| {
            world.resource::<RendererError>().0.is_some()
        });
        assert_eq!(layers(&mut app), (0, 0));

        // The placeholder now owns the bounds; the renderer leaves them.
        let theirs = Some(Rect::new(1.0, 2.0, 3.0, 4.0));
        app.world_mut().resource_mut::<AvatarBounds>().0 = theirs;
        app.update();
        app.update();
        assert_eq!(app.world().resource::<AvatarBounds>().0, theirs);
        assert_eq!(layers(&mut app), (0, 0));
    }

    #[test]
    fn idle_frames_leave_the_images_unchanged() {
        #[derive(Resource, Default)]
        struct Changes(usize);
        fn count(images: Res<SpriteImages>, mut changes: ResMut<Changes>) {
            if images.is_changed() {
                changes.0 += 1;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut app = drawn(dir.path());
        app.init_resource::<Changes>()
            .add_systems(PostUpdate, count);
        app.update(); // the first run sees the resource as new
        app.world_mut().resource_mut::<Changes>().0 = 0;
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(app.world().resource::<Changes>().0, 0);
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
