//! `background_task`: lets the model see and stop the background tasks
//! the current session started (e.g. subagents and shell commands run
//! with `background`), and peek at a running command's output.
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

/// List, peek at or cancel this session's running background tasks.
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
                description: "List, peek at or cancel the background tasks this \
                    session has running (for example subagents and shell commands \
                    started with `background`). `list` shows each task's id, what it \
                    runs and how long ago it started; `output` shows a running \
                    command's output so far; `cancel` stops one by id. A cancelled \
                    task still reports back as cancelled on a later turn."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "output", "cancel"],
                            "description": "`list` running tasks, show a running command's `output` so far, or `cancel` one."
                        },
                        "id": {
                            "type": "string",
                            "description": "Task id from `list`; required for `output` and `cancel`."
                        }
                    },
                    "required": ["action"]
                }),
            },
        }
    }
}

/// The caller's session and its turn context, or an error when the turn
/// has no session to scope tasks to.
fn session_turn() -> Result<(String, std::sync::Arc<crate::serve::TurnContext>)> {
    let Some(ctx) = crate::serve::current_turn_context() else {
        bail!("background_task is only available inside a conversation turn");
    };
    let Some(session) = ctx.session_id.clone() else {
        bail!("background_task needs a session; this turn has none");
    };
    Ok((session, ctx))
}

/// The caller's session and its task registry.
fn session_tasks() -> Result<(String, std::sync::Arc<BackgroundTasks>)> {
    let (session, ctx) = session_turn()?;
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
            "output" => {
                let Some(id) = input["id"].as_str().filter(|s| !s.is_empty()) else {
                    bail!("`output` needs an `id`; use `list` to see the running ids");
                };
                let (session, ctx) = session_turn()?;
                let finished =
                    format!("{id} has already exited; its result is in this conversation.");
                let pending = ctx.state.background.pending(&session);
                let Some(task) = pending.iter().find(|t| t.id == id) else {
                    if ctx.state.background.finished_command(&session, id) {
                        return Ok(finished);
                    }
                    let held: Vec<&str> = pending.iter().map(|t| t.id.as_str()).collect();
                    bail!(
                        "no running background task {id}; running: {}",
                        if held.is_empty() {
                            "none".to_string()
                        } else {
                            held.join(", ")
                        }
                    );
                };
                if !matches!(task.kind, TaskKind::Shell { .. }) {
                    return Ok(format!(
                        "{id} is not a command; output works only on background shell commands."
                    ));
                }
                match ctx.state.shell_runs.peek(id).await {
                    Some(text) => {
                        let mut text = text?;
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str("(still running)");
                        Ok(text)
                    }
                    // Pending but no longer running: it exited between the
                    // two looks, and its completion is on the way.
                    None => Ok(finished),
                }
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
            other => bail!("unknown action {other:?}; use \"list\", \"output\" or \"cancel\""),
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

    fn shell_kind() -> TaskKind {
        TaskKind::Shell {
            command: "cargo test".into(),
            on: ShellHost::Agent,
            timeout_secs: 1800,
        }
    }

    #[tokio::test]
    async fn output_peeks_a_running_command() {
        use crate::tools::background_shell::{LiveRun, TailBuffer};
        let state = ServeState::for_test(false);
        let id = state
            .background
            .register("s", shell_kind(), "cargo test".into())
            .unwrap();
        let mut buf = TailBuffer::new(1000);
        buf.push(b"Compiling foo\n");
        state.shell_runs.insert(
            id.clone(),
            LiveRun::Agent {
                output: Arc::new(std::sync::Mutex::new(buf)),
                pgid: None,
            },
        );
        let out = run(&state, Some("s"), json!({"action": "output", "id": id}))
            .await
            .unwrap();
        assert_eq!(out, "Compiling foo\n(still running)");
    }

    #[tokio::test]
    async fn output_on_a_finished_command() {
        let state = ServeState::for_test(false);
        let id = state
            .background
            .register("s", shell_kind(), "cargo test".into())
            .unwrap();
        let finished = format!("{id} has already exited; its result is in this conversation.");

        // Still pending but no longer running: the peek raced the exit.
        let out = run(&state, Some("s"), json!({"action": "output", "id": &id}))
            .await
            .unwrap();
        assert_eq!(out, finished);

        // Completed and delivered: no longer pending at all.
        assert!(state.background.complete(
            "s",
            &id,
            crate::background::Outcome::Exited {
                exit_code: Some(0),
                output: "ok".into(),
                timed_out: false,
            },
        ));
        assert_eq!(state.background.drain("s").len(), 1);
        state.background.commit_delivery("s");
        assert!(state.background.pending("s").is_empty());
        let out = run(&state, Some("s"), json!({"action": "output", "id": &id}))
            .await
            .unwrap();
        assert_eq!(out, finished);

        // Only the session that owns it knows it.
        let err = run(
            &state,
            Some("other"),
            json!({"action": "output", "id": &id}),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("no running background task {id}")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn output_on_a_cancelled_command() {
        let state = ServeState::for_test(false);
        let id = state
            .background
            .register("s", shell_kind(), "cargo test".into())
            .unwrap();
        state.background.cancel("s", &id).unwrap();
        let out = run(&state, Some("s"), json!({"action": "output", "id": &id}))
            .await
            .unwrap();
        assert_eq!(
            out,
            format!("{id} has already exited; its result is in this conversation.")
        );

        // Teardown forgets it with the session.
        state.background.cancel_all("s");
        assert!(!state.background.finished_command("s", &id));
    }

    #[tokio::test]
    async fn output_on_a_subagent_task() {
        let state = ServeState::for_test(false);
        let id = state.background.register("s", kind(), "x".into()).unwrap();
        let out = run(&state, Some("s"), json!({"action": "output", "id": id}))
            .await
            .unwrap();
        assert_eq!(
            out,
            format!("{id} is not a command; output works only on background shell commands.")
        );
    }

    #[tokio::test]
    async fn output_needs_an_id() {
        let state = ServeState::for_test(false);
        let err = run(&state, Some("s"), json!({"action": "output"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[tokio::test]
    async fn output_on_an_unknown_id_is_an_error() {
        let state = ServeState::for_test(false);
        let err = run(&state, Some("s"), json!({"action": "output", "id": "nope"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no running background task nope"),
            "{err}"
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
