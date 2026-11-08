//! `background_task`: lets the model see and stop the background tasks
//! the current session started (e.g. subagents run with `background`).
//!
//! The registry is [`crate::background::BackgroundTasks`], reached through
//! the turn context like `subagent` does, so a session only ever sees and
//! cancels its own tasks.

use crate::background::{BackgroundTasks, ShellHost, TaskKind};
use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};
use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

/// List or cancel this session's running background tasks.
pub struct BackgroundTaskTool {
    spec: ToolSpec,
}

impl Default for BackgroundTaskTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "background_task".into(),
                description: "List or cancel the background tasks this session has \
                    running (for example subagents started with `background`). \
                    `list` shows each task's id, what it runs and how long ago it \
                    started; `cancel` stops one by id. A cancelled task still \
                    reports back as cancelled on a later turn."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "cancel"],
                            "description": "`list` running tasks or `cancel` one."
                        },
                        "id": {
                            "type": "string",
                            "description": "Task id from `list`; required for `cancel`."
                        }
                    },
                    "required": ["action"]
                }),
            },
        }
    }
}

/// The caller's session and its task registry, or an error when the turn
/// has no session to scope tasks to.
fn session_tasks() -> Result<(String, std::sync::Arc<BackgroundTasks>)> {
    let Some(ctx) = crate::serve::current_turn_context() else {
        bail!("background_task is only available inside a conversation turn");
    };
    let Some(session) = ctx.session_id.clone() else {
        bail!("background_task needs a session; this turn has none");
    };
    Ok((session, std::sync::Arc::clone(&ctx.state.background)))
}

#[async_trait]
impl Tool for BackgroundTaskTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Think
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let action = input["action"].as_str().unwrap_or_default();
        match action {
            "list" => {
                let (session, tasks) = session_tasks()?;
                let pending = tasks.pending(&session);
                if pending.is_empty() {
                    return Ok("No background tasks are running.".to_string());
                }
                let now = Utc::now();
                let rows: Vec<String> = pending
                    .iter()
                    .map(|t| {
                        let what = match &t.kind {
                            TaskKind::Subagent { agent, .. } => format!("subagent {agent}"),
                            TaskKind::Question { question, .. } => {
                                format!("question \"{}\"", question.lines().next().unwrap_or(""))
                            }
                            TaskKind::Shell { command, on, .. } => {
                                let on = match on {
                                    ShellHost::Agent => "agent",
                                    ShellHost::Editor => "editor",
                                };
                                format!(
                                    "command \"{}\" on {on}",
                                    crate::background::first_line(command)
                                )
                            }
                        };
                        let secs = (now - t.started_at).num_seconds().max(0);
                        format!("- {} ({what}): {} — started {secs}s ago", t.id, t.label)
                    })
                    .collect();
                Ok(rows.join("\n"))
            }
            "cancel" => {
                let Some(id) = input["id"].as_str().filter(|s| !s.is_empty()) else {
                    bail!("`cancel` needs an `id`; use `list` to see the running ids");
                };
                let (session, tasks) = session_tasks()?;
                let task = tasks.cancel(&session, id)?;
                if matches!(task.kind, TaskKind::Question { .. })
                    && let Some(ctx) = crate::serve::current_turn_context()
                    && let Some(consult) = ctx.state.consult()
                {
                    consult.withdraw(vec![task]).await;
                }
                Ok(format!("Cancelled background task {id}."))
            }
            other => bail!("unknown action {other:?}; use \"list\" or \"cancel\""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::{NullProgress, ServeState, TurnContext};
    use std::sync::Arc;

    fn ctx(state: &Arc<ServeState>, session: Option<&str>) -> Arc<TurnContext> {
        Arc::new(TurnContext {
            state: Arc::clone(state),
            provider: Arc::new(crate::serve::StubProvider::new(Vec::new())),
            progress: Arc::new(NullProgress),
            visible_specs: Vec::<ToolSpec>::new().into(),
            timer_origin: None,
            session_id: session.map(str::to_string),
            subagent_depth: 0,
            admin_room_profile: None,
        })
    }

    fn kind() -> TaskKind {
        TaskKind::Subagent {
            agent: "impl".into(),
            handle: "h1".into(),
        }
    }

    async fn run(
        state: &Arc<ServeState>,
        session: Option<&str>,
        input: serde_json::Value,
    ) -> Result<String> {
        crate::serve::scope_turn_context(
            ctx(state, session),
            BackgroundTaskTool::new().execute(&input),
        )
        .await
    }

    #[tokio::test]
    async fn list_shows_id_label_and_elapsed() {
        let state = ServeState::for_test(false);
        let empty = run(&state, Some("s"), json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(empty, "No background tasks are running.");

        let id = state
            .background
            .register("s", kind(), "write the parser".into())
            .unwrap();
        let out = run(&state, Some("s"), json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.contains(&id), "{out}");
        assert!(out.contains("write the parser"), "{out}");
        assert!(out.contains("s ago"), "{out}");
    }

    #[tokio::test]
    async fn cancel_stops_a_task_and_queues_cancelled() {
        let state = ServeState::for_test(false);
        let id = state.background.register("s", kind(), "x".into()).unwrap();
        run(&state, Some("s"), json!({"action": "cancel", "id": id}))
            .await
            .unwrap();
        assert!(state.background.pending("s").is_empty());
        let done = state.background.drain("s");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].outcome, crate::background::Outcome::Cancelled);

        let err = run(&state, Some("s"), json!({"action": "cancel", "id": "nope"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn cancelling_a_question_posts_the_withdrawal() {
        let mut state = ServeState::for_test(false);
        let f = crate::consult::test_support::Fixture::new();
        // The tool cancels in the state's registry, which is the consult's.
        Arc::get_mut(&mut state).unwrap().background = Arc::clone(&f.background);
        state.set_consult(f.consult.clone());
        let id = f.consult.ask("s", "t", None, "why?").await.unwrap();
        let out = run(&state, Some("s"), json!({"action": "cancel", "id": id}))
            .await
            .unwrap();
        assert_eq!(out, format!("Cancelled background task {id}."));
        let posts = f.channel.posts();
        assert_eq!(posts.len(), 2, "{posts:?}");
        assert_eq!(posts[1].content, "This question was withdrawn.");
        assert_eq!(
            f.background.route_reply("room", "m1"),
            crate::background::Route::Unknown
        );
    }

    #[tokio::test]
    async fn cancel_without_id_is_an_error() {
        let state = ServeState::for_test(false);
        let err = run(&state, Some("s"), json!({"action": "cancel"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[tokio::test]
    async fn unknown_action_is_an_error() {
        let state = ServeState::for_test(false);
        let err = run(&state, Some("s"), json!({"action": "pause"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn refused_without_a_session() {
        let state = ServeState::for_test(false);
        let err = run(&state, None, json!({"action": "list"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("session"), "{err}");
    }
}
