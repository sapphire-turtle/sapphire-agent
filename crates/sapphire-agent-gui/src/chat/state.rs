//! Chat state and its reducer.
//!
//! Nothing here touches bevy beyond deriving `Resource` on [`ChatState`],
//! so the rules — what an event does to the chat, when a draft may be
//! sent — are plain functions with unit tests.

use bevy::prelude::Resource;

use super::bridge::BridgeEvent;

/// The agent's tools that move the avatar. They are how the avatar shows
/// itself, not work to wait on, so they never show as a status.
const AVATAR_TOOLS: [&str; 3] = ["set_expression", "play_action", "set_pose"];

fn is_avatar_tool(name: &str) -> bool {
    AVATAR_TOOLS.contains(&name)
}

/// All of the chat screen's state in one resource.
#[derive(Debug, Resource)]
pub struct ChatState {
    pub session: Session,
    pub turn: TurnState,
    pub history: Vec<ChatEntry>,
    pub draft: String,
    pub last_status: Option<String>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatState {
    /// Connecting, with an empty history and draft.
    pub fn new() -> Self {
        Self {
            session: Session::Connecting,
            turn: TurnState::Idle,
            history: Vec::new(),
            draft: String::new(),
            last_status: None,
        }
    }

    /// Fold one event from the bridge into the state.
    pub fn apply(&mut self, event: BridgeEvent) {
        match event {
            BridgeEvent::SessionReady { display_id } => {
                self.session = Session::Ready { display_id };
            }
            BridgeEvent::SessionFailed { message } => {
                self.session = Session::Failed { message };
            }
            BridgeEvent::ChatText { text } => {
                self.history.push(ChatEntry {
                    role: ChatRole::Assistant,
                    text,
                });
                self.turn = TurnState::Idle;
                self.last_status = None;
            }
            BridgeEvent::ChatError { message } => {
                self.history.push(ChatEntry {
                    role: ChatRole::System,
                    text: format!("error: {message}"),
                });
                self.turn = TurnState::Idle;
                self.last_status = None;
            }
            BridgeEvent::ToolStart { name } | BridgeEvent::ToolEnd { name }
                if is_avatar_tool(&name) => {}
            BridgeEvent::ToolStart { name } => {
                self.last_status = Some(format!("tool: {name}…"));
            }
            BridgeEvent::ToolEnd { name } => {
                self.last_status = Some(format!("tool: {name} done"));
            }
            // The avatar's cues; `drain_bridge_events` hands them to it.
            BridgeEvent::Expression { .. }
            | BridgeEvent::Action { .. }
            | BridgeEvent::Pose { .. } => {}
        }
    }

    /// Take the draft as the next turn, if one may be sent now: the session
    /// is ready, no turn is in flight and the draft isn't blank. The draft
    /// becomes a `User` entry and the turn starts `Sending`; the caller
    /// hands the returned text to the bridge.
    pub fn take_submission(&mut self) -> Option<String> {
        let ready = matches!(self.session, Session::Ready { .. });
        if !ready || self.turn != TurnState::Idle || self.draft.trim().is_empty() {
            return None;
        }
        let content = std::mem::take(&mut self.draft);
        self.history.push(ChatEntry {
            role: ChatRole::User,
            text: content.clone(),
        });
        self.turn = TurnState::Sending;
        self.last_status = None;
        Some(content)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_appends_assistant_and_idles() {
        let mut s = ChatState::new();
        s.turn = TurnState::Sending;
        s.last_status = Some("tool: x…".into());
        s.apply(BridgeEvent::ChatText { text: "hi".into() });
        assert_eq!(s.history.last().unwrap().role, ChatRole::Assistant);
        assert_eq!(s.history.last().unwrap().text, "hi");
        assert_eq!(s.turn, TurnState::Idle);
        assert!(s.last_status.is_none());
    }

    #[test]
    fn error_appends_system_entry() {
        let mut s = ChatState::new();
        s.turn = TurnState::Sending;
        s.apply(BridgeEvent::ChatError {
            message: "boom".into(),
        });
        assert_eq!(s.history.last().unwrap().role, ChatRole::System);
        assert_eq!(s.history.last().unwrap().text, "error: boom");
        assert_eq!(s.turn, TurnState::Idle);
    }

    #[test]
    fn tool_events_set_status() {
        let mut s = ChatState::new();
        s.apply(BridgeEvent::ToolStart {
            name: "search".into(),
        });
        assert_eq!(s.last_status.as_deref(), Some("tool: search…"));
        s.apply(BridgeEvent::ToolEnd {
            name: "search".into(),
        });
        assert_eq!(s.last_status.as_deref(), Some("tool: search done"));
    }

    #[test]
    fn avatar_tool_calls_do_not_show_as_thinking() {
        let mut s = ChatState::new();
        for name in ["set_expression", "play_action", "set_pose"] {
            s.apply(BridgeEvent::ToolStart { name: name.into() });
            assert_eq!(s.last_status, None, "{name} started");
            s.apply(BridgeEvent::ToolEnd { name: name.into() });
            assert_eq!(s.last_status, None, "{name} ended");
        }
    }

    #[test]
    fn session_events_set_session() {
        let mut s = ChatState::new();
        s.apply(BridgeEvent::SessionReady {
            display_id: "abc".into(),
        });
        assert!(matches!(&s.session, Session::Ready { display_id } if display_id == "abc"));
        s.apply(BridgeEvent::SessionFailed {
            message: "m".into(),
        });
        assert!(matches!(&s.session, Session::Failed { message } if message == "m"));
    }

    #[test]
    fn take_submission_requires_ready_idle_and_text() {
        let mut s = ChatState::new();
        s.draft = "hello".into();
        assert_eq!(s.take_submission(), None); // Connecting
        s.session = Session::Ready {
            display_id: "a".into(),
        };
        s.draft = "   ".into();
        assert_eq!(s.take_submission(), None);
        s.draft = "hello".into();
        assert_eq!(s.take_submission().as_deref(), Some("hello"));
        assert_eq!(s.turn, TurnState::Sending);
        assert!(s.draft.is_empty());
        assert_eq!(s.history.last().unwrap().role, ChatRole::User);
        s.draft = "again".into();
        assert_eq!(s.take_submission(), None); // Sending
    }
}
