//! Pure (bevy-free) application state.
//!
//! Kept independent of bevy types so future mobile / cross-platform GUI
//! crates (`sapphire-agent-mobile`, `sapphire-agent-gui`) can reuse it.

/// Connection lifecycle for the agent session.
#[derive(Debug, Clone)]
pub enum Session {
    /// Background task is connecting to the local agent and opening a
    /// session.
    Connecting,
    /// Session is ready; chat input is enabled.
    Ready { display_id: String },
    /// Connecting failed — typically the agent isn't running on this
    /// host. The message is surfaced in the top bar with a retry button.
    Failed { message: String },
}

/// A single message in the on-screen chat history. Tool calls aren't
/// persisted here — they're transient UI affordances rendered separately
/// while a turn is in flight.
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role: ChatRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    /// System-level notice (e.g. a connection or turn error). Rendered in
    /// a muted style.
    System,
}

/// In-flight turn state. Disabled input until the server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Sending,
}
