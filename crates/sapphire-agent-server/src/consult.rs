//! The consult room: autonomous sessions post questions to a chat room and
//! a human's reply to the posted message answers them.
//!
//! Platform-neutral: this module only knows [`Channels`], never which
//! platform serves the room. The bookkeeping (open questions, routing a
//! reply, the inbox) lives in [`BackgroundTasks`]; this is the part that
//! talks to the room and keeps the deadline timers.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::background::{
    BackgroundTasks, MAX_OPEN_QUESTIONS, PendingTask, Route, TaskId, TaskKind,
};
use crate::channel::{Channels, IncomingMessage, OutgoingMessage};

const NOTE_ENDED: &str = "That question has ended (withdrawn, or its session finished).";
const NOTE_NOT_A_REPLY: &str = "Reply to a question to answer it.";
const NOTE_DEADLINE: &str = "No answer by the deadline; the session will decide on its own. A reply now still reaches it as extra information.";
const NOTE_WITHDRAWN: &str = "This question was withdrawn.";
const NOTE_IMAGE: &str = "(an image was attached but not forwarded)";

/// The consult room and the questions waiting in it.
pub(crate) struct Consult {
    channels: Arc<Channels>,
    background: Arc<BackgroundTasks>,
    room: String,
    timeout: Duration,
}

impl Consult {
    pub(crate) fn new(
        channels: Arc<Channels>,
        background: Arc<BackgroundTasks>,
        room: String,
        timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels,
            background,
            room,
            timeout,
        })
    }

    pub(crate) fn is_consult_room(&self, room_id: &str) -> bool {
        self.room == room_id
    }

    /// Post `question` to the room and register it as waiting for `owner_session`.
    /// A refusal posts nothing.
    pub(crate) async fn ask(
        self: &Arc<Self>,
        owner_session: &str,
        task: &str,
        asked_by: Option<&str>,
        question: &str,
    ) -> Result<TaskId> {
        if !self.channels.serves(&self.room).await {
            bail!("consult room {} is not served on this host", self.room);
        }
        if self.channels.supports_replies(&self.room).await != Some(true) {
            bail!(
                "consult room {} is on a channel that cannot match replies",
                self.room
            );
        }
        if self.background.open_questions(owner_session) >= MAX_OPEN_QUESTIONS {
            bail!(
                "You already have 3 questions waiting for an answer. \
                 Wait for one, or withdraw one with background_task."
            );
        }

        let deadline = Utc::now()
            + chrono::Duration::from_std(self.timeout)
                .unwrap_or_else(|_| chrono::Duration::days(1));
        let who = asked_by
            .map(|h| format!(" question from subagent {h}"))
            .unwrap_or_default();
        let text = format!(
            "[Autonomous: {task}]{who}\n{question}\n(Reply to this message to answer. Waiting until {}.)",
            deadline
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
        );
        let message_ids = self
            .channels
            .send(&OutgoingMessage::new(text, &self.room))
            .await?;
        let Some(first) = message_ids.first().cloned() else {
            bail!("the consult room accepted the question but returned no message ID");
        };
        let label = question.lines().next().unwrap_or_default().to_string();
        let id = self.background.register_question(
            owner_session,
            TaskKind::Question {
                room: self.room.clone(),
                message_ids,
                question: question.to_string(),
                asked_by: asked_by.map(str::to_string),
                deadline,
            },
            label,
        )?;
        self.spawn_timer(owner_session.to_string(), id.clone(), first, deadline);
        Ok(id)
    }

    /// Route a message posted in the consult room. Only called for messages
    /// in that room.
    pub(crate) async fn handle_incoming(&self, msg: &IncomingMessage) {
        let Some(reply_to) = msg.reply_to.as_deref() else {
            self.note(NOTE_NOT_A_REPLY, &msg.id).await;
            return;
        };
        match self.background.route_reply(&self.room, reply_to) {
            Route::Unknown => self.note(NOTE_ENDED, &msg.id).await,
            Route::Pending { session, id } => {
                let Some(text) = answer_text(msg) else { return };
                let from = msg.sender.clone();
                // A deadline or withdrawal may have won the race; the reply
                // then still reaches the session as a follow-up.
                if !self
                    .background
                    .answer(&session, &id, text.clone(), from.clone())
                {
                    self.background.follow_up(&session, &id, text, from);
                }
            }
            Route::Answered { session, id } => {
                let Some(text) = answer_text(msg) else { return };
                self.background
                    .follow_up(&session, &id, text, msg.sender.clone());
            }
        }
    }

    /// Tell the room that these questions are withdrawn.
    pub(crate) async fn withdraw(&self, questions: Vec<PendingTask>) {
        for q in questions {
            if let TaskKind::Question { message_ids, .. } = &q.kind
                && let Some(first) = message_ids.first()
            {
                self.note(NOTE_WITHDRAWN, first).await;
            }
        }
    }

    /// Start a deadline timer for every pending question (after a restart).
    pub(crate) fn rearm(self: &Arc<Self>) {
        for (session, task) in self.background.pending_questions() {
            if let TaskKind::Question {
                message_ids,
                deadline,
                ..
            } = &task.kind
                && let Some(first) = message_ids.first()
            {
                self.spawn_timer(session, task.id.clone(), first.clone(), *deadline);
            }
        }
    }

    fn spawn_timer(
        self: &Arc<Self>,
        session: String,
        id: TaskId,
        first_message: String,
        deadline: DateTime<Utc>,
    ) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let wait = (deadline - Utc::now()).to_std().unwrap_or_default();
            tokio::time::sleep(wait).await;
            if this.background.expire(&session, &id) {
                this.note(NOTE_DEADLINE, &first_message).await;
            }
        });
    }

    /// Post `text` in the room as a reply to `reply_to`; a failure is only logged.
    async fn note(&self, text: &str, reply_to: &str) {
        let msg = OutgoingMessage::new(text, &self.room).replying_to(reply_to);
        if let Err(e) = self.channels.send(&msg).await {
            warn!("consult room note not posted: {e:#}");
        }
    }
}

/// What a reply delivers: its text, plus a note when images came along.
/// `None` for an empty message.
fn answer_text(msg: &IncomingMessage) -> Option<String> {
    let text = msg.content.trim();
    match (text.is_empty(), msg.attachments.is_empty()) {
        (true, true) => None,
        (true, false) => Some(NOTE_IMAGE.to_string()),
        (false, true) => Some(text.to_string()),
        (false, false) => Some(format!("{text}\n{NOTE_IMAGE}")),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::channel::Channel;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A channel that records posts, hands out IDs `m1, m2, ...` and splits
    /// a message into two posts when it contains `<split>`.
    pub(crate) struct FakeChannel {
        posts: Mutex<Vec<OutgoingMessage>>,
        next: AtomicUsize,
        replies: AtomicBool,
    }

    impl FakeChannel {
        pub(crate) fn posts(&self) -> Vec<OutgoingMessage> {
            self.posts.lock().unwrap().clone()
        }

        pub(crate) fn set_supports_replies(&self, on: bool) {
            self.replies.store(on, Ordering::SeqCst);
        }

        fn next_id(&self) -> String {
            format!("m{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    #[async_trait::async_trait]
    impl Channel for FakeChannel {
        fn name(&self) -> &str {
            "fake"
        }

        async fn send(&self, message: &OutgoingMessage) -> Result<Vec<String>> {
            self.posts.lock().unwrap().push(message.clone());
            let parts = if message.content.contains("<split>") {
                2
            } else {
                1
            };
            Ok((0..parts).map(|_| self.next_id()).collect())
        }

        fn supports_replies(&self) -> bool {
            self.replies.load(Ordering::SeqCst)
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<IncomingMessage>) -> Result<()> {
            Ok(())
        }
    }

    /// A [`Consult`] on room `"room"`, served by a [`FakeChannel`], over an
    /// in-memory [`BackgroundTasks`].
    pub(crate) struct Fixture {
        pub(crate) consult: Arc<Consult>,
        pub(crate) background: Arc<BackgroundTasks>,
        pub(crate) channel: Arc<FakeChannel>,
        channels: Arc<Channels>,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Self::with_timeout(Duration::from_secs(24 * 3600))
        }

        pub(crate) fn with_timeout(timeout: Duration) -> Self {
            Self::over(BackgroundTasks::in_memory(4), timeout)
        }

        /// A fixture whose questions live in `background`, for a test that
        /// already has a state with its own tasks.
        pub(crate) fn over(background: Arc<BackgroundTasks>, timeout: Duration) -> Self {
            let channel = Arc::new(FakeChannel {
                posts: Mutex::new(Vec::new()),
                next: AtomicUsize::new(0),
                replies: AtomicBool::new(true),
            });
            let channels = Arc::new(Channels::new(
                vec![("fake".to_string(), channel.clone() as Arc<dyn Channel>)],
                HashMap::from([("room".to_string(), "fake".to_string())]),
            ));
            let consult =
                Consult::new(channels.clone(), background.clone(), "room".into(), timeout);
            Self {
                consult,
                background,
                channel,
                channels,
            }
        }

        /// A second [`Consult`] over the same channels and tasks, for `room`.
        pub(crate) fn consult_for_room(&self, room: &str) -> Arc<Consult> {
            Consult::new(
                self.channels.clone(),
                self.background.clone(),
                room.into(),
                Duration::from_secs(3600),
            )
        }
    }

    /// A message from `@alice` in the consult room.
    pub(crate) fn incoming(id: &str, text: &str, reply_to: Option<&str>) -> IncomingMessage {
        IncomingMessage {
            id: id.into(),
            sender: "@alice".into(),
            content: text.into(),
            room_id: "room".into(),
            timestamp: 0,
            thread_id: None,
            attachments: Vec::new(),
            reply_to: reply_to.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::background::{Outcome, TaskKind};
    use std::time::Duration;

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached in 2s");
    }

    #[tokio::test]
    async fn ask_posts_and_registers() {
        let f = Fixture::new();
        f.consult.ask("s", "t", None, "why?").await.unwrap();
        let posts = f.channel.posts();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].content.starts_with(
            "[Autonomous: t]\nwhy?\n(Reply to this message to answer. Waiting until "
        ));
        assert!(posts[0].content.ends_with(".)"));
        assert_eq!(posts[0].room_id, "room");
        let pending = f.background.pending_questions();
        assert_eq!(pending.len(), 1);
        match &pending[0].1.kind {
            TaskKind::Question { message_ids, .. } => assert_eq!(message_ids, &["m1"]),
            k => panic!("{k:?}"),
        }
        assert_eq!(pending[0].1.label, "why?");
    }

    #[tokio::test]
    async fn child_ask_names_the_subagent() {
        let f = Fixture::new();
        f.consult.ask("s", "t", Some("h-1"), "why?").await.unwrap();
        let first = f.channel.posts()[0]
            .content
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(first, "[Autonomous: t] question from subagent h-1");
    }

    #[tokio::test]
    async fn reply_answers_and_bot_stays_silent() {
        let f = Fixture::new();
        f.consult.ask("s", "t", None, "why?").await.unwrap();
        f.consult
            .handle_incoming(&incoming("u1", "because", Some("m1")))
            .await;
        assert_eq!(f.channel.posts().len(), 1);
        let inbox = f.background.drain("s");
        assert_eq!(inbox.len(), 1);
        assert_eq!(
            inbox[0].outcome,
            Outcome::Answered {
                text: "because".into(),
                from: "@alice".into()
            }
        );
    }

    #[tokio::test]
    async fn reply_to_a_later_part_answers() {
        let f = Fixture::new();
        f.consult.ask("s", "t", None, "a<split>b").await.unwrap();
        match &f.background.pending_questions()[0].1.kind {
            TaskKind::Question { message_ids, .. } => assert_eq!(message_ids, &["m1", "m2"]),
            k => panic!("{k:?}"),
        }
        f.consult
            .handle_incoming(&incoming("u1", "yes", Some("m2")))
            .await;
        assert!(matches!(
            f.background.drain("s")[0].outcome,
            Outcome::Answered { .. }
        ));
    }

    #[tokio::test]
    async fn second_reply_is_a_follow_up() {
        let f = Fixture::new();
        f.consult.ask("s", "t", None, "why?").await.unwrap();
        f.consult
            .handle_incoming(&incoming("u1", "one", Some("m1")))
            .await;
        f.consult
            .handle_incoming(&incoming("u2", "two", Some("m1")))
            .await;
        assert_eq!(f.channel.posts().len(), 1);
        let inbox = f.background.drain("s");
        assert_eq!(inbox.len(), 2);
        assert_eq!(
            inbox[1].outcome,
            Outcome::FollowUp {
                text: "two".into(),
                from: "@alice".into()
            }
        );
    }

    #[tokio::test]
    async fn unknown_reply_and_non_reply_get_notes() {
        let f = Fixture::new();
        f.consult.handle_incoming(&incoming("u1", "hi", None)).await;
        f.consult
            .handle_incoming(&incoming("u2", "hi", Some("zzz")))
            .await;
        let posts = f.channel.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].content, NOTE_NOT_A_REPLY);
        assert_eq!(posts[0].reply_to.as_deref(), Some("u1"));
        assert_eq!(posts[1].content, NOTE_ENDED);
        assert_eq!(posts[1].reply_to.as_deref(), Some("u2"));
    }

    #[tokio::test]
    async fn image_only_reply_delivers_the_note() {
        let f = Fixture::new();
        f.consult.ask("s", "t", None, "a").await.unwrap();
        f.consult.ask("s", "t", None, "b").await.unwrap();
        let mut img = incoming("u1", "", Some("m1"));
        img.attachments.push(crate::channel::Attachment {
            media_type: "image/png".into(),
            data: vec![1],
        });
        f.consult.handle_incoming(&img).await;
        let mut both = incoming("u2", "look", Some("m2"));
        both.attachments = img.attachments.clone();
        f.consult.handle_incoming(&both).await;
        let inbox = f.background.drain("s");
        let texts: Vec<_> = inbox
            .iter()
            .map(|c| match &c.outcome {
                Outcome::Answered { text, .. } => text.clone(),
                o => panic!("{o:?}"),
            })
            .collect();
        assert_eq!(texts[0], "(an image was attached but not forwarded)");
        assert_eq!(texts[1], "look\n(an image was attached but not forwarded)");
    }

    #[tokio::test]
    async fn routes_to_the_asking_session() {
        let f = Fixture::new();
        f.consult.ask("A", "t", None, "qa").await.unwrap();
        f.consult.ask("B", "t", None, "qb").await.unwrap();
        f.consult
            .handle_incoming(&incoming("u1", "for b", Some("m2")))
            .await;
        f.consult
            .handle_incoming(&incoming("u2", "for a", Some("m1")))
            .await;
        let a = f.background.drain("A");
        let b = f.background.drain("B");
        assert!(matches!(&a[0].outcome, Outcome::Answered { text, .. } if text == "for a"));
        assert!(matches!(&b[0].outcome, Outcome::Answered { text, .. } if text == "for b"));
    }

    #[tokio::test]
    async fn deadline_expires_and_notes() {
        let f = Fixture::with_timeout(Duration::from_millis(10));
        f.consult.ask("s", "t", None, "why?").await.unwrap();
        wait_for(|| f.channel.posts().len() == 2).await;
        let inbox = f.background.drain("s");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].outcome, Outcome::NoAnswer);
        let note = &f.channel.posts()[1];
        assert_eq!(note.content, NOTE_DEADLINE);
        assert_eq!(note.reply_to.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn a_late_timer_after_teardown_is_silent() {
        let f = Fixture::with_timeout(Duration::from_millis(30));
        f.consult.ask("s", "t", None, "why?").await.unwrap();
        let withdrawn = f.background.cancel_all("s");
        f.consult.withdraw(withdrawn).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        let posts = f.channel.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1].content, "This question was withdrawn.");
        assert_eq!(posts[1].reply_to.as_deref(), Some("m1"));
        assert!(f.background.drain("s").is_empty());
    }

    #[tokio::test]
    async fn rearm_expires_past_deadlines_once() {
        let f = Fixture::new();
        f.background
            .register_question(
                "s",
                TaskKind::Question {
                    room: "room".into(),
                    message_ids: vec!["m1".into()],
                    question: "why?".into(),
                    asked_by: None,
                    deadline: chrono::Utc::now() - chrono::Duration::hours(1),
                },
                "why?".into(),
            )
            .unwrap();
        f.consult.rearm();
        wait_for(|| f.channel.posts().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let inbox = f.background.drain("s");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].outcome, Outcome::NoAnswer);
        assert_eq!(f.channel.posts().len(), 1);
        assert_eq!(f.channel.posts()[0].content, NOTE_DEADLINE);
    }

    #[tokio::test]
    async fn refusals_post_nothing() {
        let f = Fixture::new();
        let err = |e: anyhow::Error| e.to_string();

        let unserved = f.consult_for_room("nowhere");
        assert_eq!(
            err(unserved.ask("s", "t", None, "q").await.unwrap_err()),
            "consult room nowhere is not served on this host"
        );

        f.channel.set_supports_replies(false);
        assert_eq!(
            err(f.consult.ask("s", "t", None, "q").await.unwrap_err()),
            "consult room room is on a channel that cannot match replies"
        );
        f.channel.set_supports_replies(true);

        for _ in 0..3 {
            f.consult.ask("s", "t", None, "q").await.unwrap();
        }
        let before = f.channel.posts().len();
        assert_eq!(
            err(f.consult.ask("s", "t", None, "q").await.unwrap_err()),
            "You already have 3 questions waiting for an answer. Wait for one, or withdraw one with background_task."
        );
        assert_eq!(f.channel.posts().len(), before);
        assert_eq!(before, 3);
    }
}
