//! What the avatar is showing: its expression, its pose and the action it
//! has yet to play, and when it falls back to neutral after a turn.
//!
//! Pure of bevy except for the [`AvatarState`] `Resource` derive. Times are
//! the time since the app started, as the system calling in measures it.

use std::time::Duration;

use bevy::prelude::Resource;

/// The expression a fresh or reverted avatar shows.
pub const NEUTRAL: &str = "neutral";

/// The cues the agent gave, as every renderer reads them. A renderer shows
/// what it can of them and ignores the rest.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct AvatarState {
    pub expression: String,
    pub intensity: f32,
    /// The pose to hold; `None` is the renderer's default pose.
    pub pose: Option<String>,
    /// An action to play once, until a renderer takes it.
    pending_action: Option<String>,
    /// When expression and pose go back to their defaults: set when a
    /// turn's reply is done, cleared when the next turn starts.
    revert_at: Option<Duration>,
}

impl Default for AvatarState {
    fn default() -> Self {
        Self::new()
    }
}

impl AvatarState {
    /// Neutral at full intensity, the default pose, nothing to play.
    pub fn new() -> Self {
        Self {
            expression: NEUTRAL.to_owned(),
            intensity: 1.0,
            pose: None,
            pending_action: None,
            revert_at: None,
        }
    }

    /// Show `name` until the next expression or the revert.
    pub fn expression(&mut self, name: impl Into<String>, intensity: f32) {
        self.expression = name.into();
        self.intensity = intensity;
    }

    /// Hold `name` until the next pose or the revert.
    pub fn pose(&mut self, name: impl Into<String>) {
        self.pose = Some(name.into());
    }

    /// Play `name` once; it replaces an action not yet taken.
    pub fn action(&mut self, name: impl Into<String>) {
        self.pending_action = Some(name.into());
    }

    /// The action waiting to be played, left in place: for a renderer that
    /// checks it can play it before taking it. Needs only `Res`.
    pub fn pending_action(&self) -> Option<&str> {
        self.pending_action.as_deref()
    }

    /// The action to play, handed out once. Needs `ResMut`, so take it only
    /// when playing it: the change it makes is one renderers react to.
    pub fn take_action(&mut self) -> Option<String> {
        self.pending_action.take()
    }

    /// A turn began: whatever the agent shows next holds through it.
    pub fn turn_started(&mut self) {
        self.revert_at = None;
    }

    /// A turn's reply is done at `now`: revert `after` it unless another
    /// turn starts first.
    pub fn turn_finished(&mut self, now: Duration, after: Duration) {
        self.revert_at = Some(now + after);
    }

    /// Revert once `now` reaches the time set by [`Self::turn_finished`].
    pub fn tick(&mut self, now: Duration) {
        if self.revert_at.is_some_and(|at| now >= at) {
            self.expression = NEUTRAL.to_owned();
            self.intensity = 1.0;
            self.pose = None;
            self.revert_at = None;
        }
    }

    /// Back to [`Self::new`], for a renderer that starts over.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn an_expression_holds_until_revert_after_the_turn() {
        let mut state = AvatarState::new();
        state.expression("happy", 0.5);
        state.turn_finished(s(10), s(30));
        state.tick(s(39));
        assert_eq!(state.expression, "happy");
        assert_eq!(state.intensity, 0.5);
        state.tick(s(40));
        assert_eq!(state.expression, "neutral");
        assert_eq!(state.intensity, 1.0);
        assert_eq!(state.pose, None);
    }

    #[test]
    fn a_new_turn_cancels_the_pending_revert() {
        let mut state = AvatarState::new();
        state.expression("happy", 1.0);
        state.turn_finished(s(0), s(30));
        state.turn_started();
        state.tick(s(100));
        assert_eq!(state.expression, "happy");
    }

    #[test]
    fn an_action_is_taken_once() {
        let mut state = AvatarState::new();
        assert_eq!(state.take_action(), None);
        state.action("wave");
        assert_eq!(state.pending_action(), Some("wave"), "a peek leaves it");
        assert_eq!(state.pending_action(), Some("wave"));
        assert_eq!(state.take_action().as_deref(), Some("wave"));
        assert_eq!(state.pending_action(), None);
        assert_eq!(state.take_action(), None);
    }

    #[test]
    fn pose_reverts_with_the_expression() {
        let mut state = AvatarState::new();
        state.pose("arms");
        assert_eq!(state.pose.as_deref(), Some("arms"));
        state.turn_finished(s(0), s(30));
        state.tick(s(30));
        assert_eq!(state.pose, None);
    }
}
