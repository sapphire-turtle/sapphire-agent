//! The stand-in when there is no avatar to draw: a neutral silhouette,
//! painted by egui in `Order::Background`, beneath the chat's area (whose
//! opaque panels cover it where they reach). It follows the renderer
//! contract like the real renderers, declaring no cues so the agent offers
//! no avatar tools.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

use super::{AvatarBounds, DeclaredAvatar};

/// Share of the window's height the silhouette stands.
const HEIGHT: f32 = 0.8;
/// Opacity of the silhouette over the theme's weak text colour.
const ALPHA: f32 = 0.4;

/// The silhouette's shapes, in egui points.
#[derive(Debug, PartialEq)]
struct Silhouette {
    /// Everything it covers: centred, standing on the bottom edge.
    bounds: egui::Rect,
    head_center: egui::Pos2,
    head_radius: f32,
    body: egui::Rect,
}

impl Silhouette {
    fn within(screen: egui::Rect) -> Self {
        let height = screen.height() * HEIGHT;
        let width = height * 0.4;
        let bounds = egui::Rect::from_min_max(
            egui::pos2(screen.center().x - width / 2.0, screen.bottom() - height),
            egui::pos2(screen.center().x + width / 2.0, screen.bottom()),
        );
        let head_radius = height * 0.12;
        let head_center = egui::pos2(bounds.center().x, bounds.top() + head_radius);
        let body = egui::Rect::from_min_max(
            egui::pos2(bounds.left(), head_center.y + head_radius + height * 0.03),
            bounds.max,
        );
        Self {
            bounds,
            head_center,
            head_radius,
            body,
        }
    }
}

/// Paints the silhouette and reports where it is.
pub(crate) fn draw(
    mut contexts: EguiContexts,
    mut bounds: ResMut<AvatarBounds>,
    mut declared: ResMut<DeclaredAvatar>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let shape = Silhouette::within(ctx.content_rect());
    egui::Area::new(egui::Id::unique("avatar_placeholder"))
        .order(egui::Order::Background)
        // Clicks go to the chat; gestures use `AvatarBounds`.
        .interactable(false)
        .fixed_pos(shape.bounds.min)
        .show(ctx, |ui| {
            let color = ui.visuals().weak_text_color().gamma_multiply(ALPHA);
            let painter = ui.painter();
            painter.circle_filled(shape.head_center, shape.head_radius, color);
            painter.rect_filled(shape.body, shape.body.width() * 0.25, color);
        });

    let rect = Some(Rect::new(
        shape.bounds.min.x,
        shape.bounds.min.y,
        shape.bounds.max.x,
        shape.bounds.max.y,
    ));
    if bounds.0 != rect {
        bounds.0 = rect;
    }
    if declared.0.is_some() {
        declared.0 = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_silhouette_stands_centred_on_the_bottom_at_80_percent_height() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 720.0));
        let s = Silhouette::within(screen);
        assert_eq!(s.bounds.height(), 576.0);
        assert_eq!(s.bounds.bottom(), 720.0);
        assert_eq!(s.bounds.center().x, 320.0);
        assert_eq!(s.head_center.y - s.head_radius, s.bounds.top());
        assert_eq!(s.body.bottom(), s.bounds.bottom());
        assert!(s.body.top() > s.head_center.y + s.head_radius);
    }
}
