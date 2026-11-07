//! `ask_human`: an autonomous session's way to put a question to the
//! person overseeing it, in the consult room.
//!
//! Returns at once. The question is a background task
//! ([`crate::background::TaskKind::Question`]); the answer arrives in the
//! session's next turn, like a subagent's report. Who may call it is
//! decided by [`crate::serve::TurnHost::consult_identity`], not here: the
//! tool is hidden from every turn whose host has none.

use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::json;

/// The tool's name, which `run_llm_turn` and `visible_tool_predicate` key on.
pub(crate) const NAME: &str = "ask_human";

const NOT_AUTONOMOUS: &str = "ask_human is only available in autonomous sessions.";

pub struct AskHumanTool {
    spec: ToolSpec,
}

impl Default for AskHumanTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AskHumanTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: NAME.into(),
                description: "Ask the person overseeing autonomous work a question in the \
                    consult room. Returns at once; the answer arrives later in this \
                    conversation. Use it for unclear requirements you cannot settle from \
                    the code or docs."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask."
                        }
                    },
                    "required": ["question"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for AskHumanTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Think
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let Some(ctx) = crate::serve::current_turn_context() else {
            bail!(NOT_AUTONOMOUS);
        };
        let Some(identity) = ctx.progress.consult_identity() else {
            bail!(NOT_AUTONOMOUS);
        };
        let question = input["question"].as_str().unwrap_or("").trim();
        if question.is_empty() {
            bail!("`question` is required");
        }
        let Some(consult) = ctx.state.consult() else {
            // Registered only with a room configured, so it is named here;
            // the channel behind it just is not up on this host.
            match ctx.state.config.autonomous.consult_room.as_deref() {
                Some(room) => bail!("consult room {room} is not served on this host"),
                None => bail!(NOT_AUTONOMOUS),
            }
        };
        let id = consult
            .ask(
                &identity.owner_session,
                &identity.task,
                identity.asked_by.as_deref(),
                question,
            )
            .await?;
        Ok(match identity.asked_by {
            None => format!(
                "Question {id} posted. The answer will be delivered to this conversation \
                 when it arrives; keep working or reply WAIT."
            ),
            Some(_) => format!(
                "Question {id} posted. The answer goes to the session that started you, \
                 not to you; finish what you can and mention the question in your report."
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consult::test_support::Fixture;
    use crate::serve::{ConsultIdentity, NullProgress, ServeState, TurnContext, TurnHost};
    use std::sync::Arc;

    struct Asker(Option<ConsultIdentity>);

    #[async_trait]
    impl TurnHost for Asker {
        async fn tool_start(&self, _id: &str, _name: &str, _input: &serde_json::Value) {}
        async fn tool_end(&self, _id: &str, _name: &str) {}
        async fn turn_error(&self, _message: &str) {}
        fn consult_identity(&self) -> Option<ConsultIdentity> {
            self.0.clone()
        }
    }

    fn identity(asked_by: Option<&str>) -> ConsultIdentity {
        ConsultIdentity {
            task: "refactor".into(),
            owner_session: "s".into(),
            asked_by: asked_by.map(str::to_string),
        }
    }

    async fn run(
        state: &Arc<ServeState>,
        progress: Arc<dyn TurnHost>,
        input: serde_json::Value,
    ) -> Result<String> {
        let ctx = Arc::new(TurnContext {
            state: Arc::clone(state),
            provider: Arc::new(crate::serve::StubProvider::new(Vec::new())),
            progress,
            visible_specs: Vec::<ToolSpec>::new().into(),
            timer_origin: None,
            session_id: Some("s".into()),
            subagent_depth: 0,
            admin_room_profile: None,
        });
        crate::serve::scope_turn_context(ctx, AskHumanTool::new().execute(&input)).await
    }

    #[tokio::test]
    async fn refuses_without_an_identity() {
        let state = ServeState::for_test(false);
        let err = run(&state, Arc::new(NullProgress), json!({"question": "q"}))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ask_human is only available in autonomous sessions."
        );
        let err = AskHumanTool::new()
            .execute(&json!({"question": "q"}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), NOT_AUTONOMOUS);
    }

    #[tokio::test]
    async fn refuses_when_the_room_is_not_served() {
        let state = ServeState::for_test(false);
        let err = run(
            &state,
            Arc::new(Asker(Some(identity(None)))),
            json!({"question": "q"}),
        )
        .await
        .unwrap_err();
        // No room configured either: the same text as no identity.
        assert_eq!(err.to_string(), NOT_AUTONOMOUS);
    }

    #[tokio::test]
    async fn names_the_configured_room_when_no_consult_is_set() {
        let state = ServeState::for_test_with_consult_room("!ops:x");
        let err = run(
            &state,
            Arc::new(Asker(Some(identity(None)))),
            json!({"question": "q"}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "consult room !ops:x is not served on this host"
        );
    }

    #[tokio::test]
    async fn posts_and_answers_for_a_session() {
        let state = ServeState::for_test(false);
        let f = Fixture::new();
        // Share the fixture's task registry with the state's.
        state.set_consult(f.consult.clone());
        let out = run(
            &state,
            Arc::new(Asker(Some(identity(None)))),
            json!({"question": "why?"}),
        )
        .await
        .unwrap();
        let id = f.background.pending_questions()[0].1.id.clone();
        assert_eq!(
            out,
            format!(
                "Question {id} posted. The answer will be delivered to this conversation \
                 when it arrives; keep working or reply WAIT."
            )
        );
        let posts = f.channel.posts();
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0]
                .content
                .starts_with("[Autonomous: refactor]\nwhy?\n")
        );
    }

    #[tokio::test]
    async fn a_child_gets_the_child_text_and_is_named() {
        let state = ServeState::for_test(false);
        let f = Fixture::new();
        state.set_consult(f.consult.clone());
        let out = run(
            &state,
            Arc::new(Asker(Some(identity(Some("h-1"))))),
            json!({"question": "why?"}),
        )
        .await
        .unwrap();
        let id = f.background.pending_questions()[0].1.id.clone();
        assert_eq!(
            out,
            format!(
                "Question {id} posted. The answer goes to the session that started you, \
                 not to you; finish what you can and mention the question in your report."
            )
        );
        assert!(
            f.channel.posts()[0]
                .content
                .starts_with("[Autonomous: refactor] question from subagent h-1\n")
        );
    }

    #[tokio::test]
    async fn an_empty_question_is_refused() {
        let state = ServeState::for_test(false);
        let err = run(
            &state,
            Arc::new(Asker(Some(identity(None)))),
            json!({"question": "  "}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("question"), "{err}");
    }
}
