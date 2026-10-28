//! The tools that give the agent a body: `set_expression` and
//! `play_action`, for a client that shows an avatar (the desktop mate).
//!
//! Which cues exist is the client's to say — a VRM model ships its own
//! expressions and gestures — so the client declares them on each turn
//! (`ClientCaps::avatar`) and nothing here keeps a list of its own. A turn
//! whose client declared nothing is offered neither tool: `run_llm_turn`
//! adds them back only when `TurnHost::avatar` answers, and
//! [`specialize_specs`] narrows their `enum`s to what was declared, so the
//! model picks from the names this client can actually play.
//!
//! The cue goes to the client as a progress event while the turn runs,
//! through an [`AvatarSink`] the transport supplies. Like the ACP client
//! (`acp_client.rs`), the turn is carried to the tool in a
//! `tokio::task_local` scoped around execution rather than threaded
//! through `Tool::execute`.

use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use sapphire_agent_core::AvatarCaps;
use serde_json::{Value, json};

use crate::provider::ToolSpec;
use crate::tools::{Tool, ToolKind};

pub(crate) const SET_EXPRESSION: &str = "set_expression";
pub(crate) const PLAY_ACTION: &str = "play_action";

/// Whether `name` is one of this module's tools — the ones only a turn
/// with a declared avatar may see.
pub(crate) fn is_avatar_tool(name: &str) -> bool {
    matches!(name, SET_EXPRESSION | PLAY_ACTION)
}

/// Where a cue goes: the client that declared the avatar.
#[async_trait]
pub(crate) trait AvatarSink: Send + Sync {
    async fn expression(&self, name: &str, intensity: f32);
    async fn action(&self, name: &str);
}

/// One turn's avatar: what the client declared, where cues go, and
/// whether this turn has spent its gesture.
pub(crate) struct AvatarTurn {
    caps: AvatarCaps,
    sink: Arc<dyn AvatarSink>,
    /// A gesture repeated within one reply reads as a tic, not as
    /// emphasis, so a turn gets one. Per turn rather than per round:
    /// the transport builds one `AvatarTurn` per turn.
    action_used: AtomicBool,
}

impl AvatarTurn {
    pub(crate) fn new(caps: AvatarCaps, sink: Arc<dyn AvatarSink>) -> Arc<Self> {
        Arc::new(Self {
            caps,
            sink,
            action_used: AtomicBool::new(false),
        })
    }

    pub(crate) fn caps(&self) -> &AvatarCaps {
        &self.caps
    }
}

tokio::task_local! {
    static AVATAR_TURN_TL: Option<Arc<AvatarTurn>>;
}

/// Run `fut` with `turn` reachable from [`current_avatar_turn`].
pub(crate) fn scope_avatar_turn<F: Future>(
    turn: Option<Arc<AvatarTurn>>,
    fut: F,
) -> impl Future<Output = F::Output> {
    AVATAR_TURN_TL.scope(turn, fut)
}

/// The avatar of the turn currently executing, if its client declared one.
pub(crate) fn current_avatar_turn() -> Option<Arc<AvatarTurn>> {
    AVATAR_TURN_TL.try_with(Clone::clone).ok().flatten()
}

/// Fit this turn's tool list to what its client declared: with no avatar
/// both tools go, with no actions `play_action` goes, and whatever stays
/// has its `enum` replaced by the declared names. An empty `enum` is a
/// schema no name can satisfy — and some providers reject outright — so a
/// declaration with no expressions drops `set_expression` the same way.
pub(crate) fn specialize_specs(specs: &mut Vec<ToolSpec>, caps: Option<&AvatarCaps>) {
    specs.retain(|s| match (s.name.as_ref(), caps) {
        (SET_EXPRESSION | PLAY_ACTION, None) => false,
        (SET_EXPRESSION, Some(c)) => !c.expressions.is_empty(),
        (PLAY_ACTION, Some(c)) => !c.actions.is_empty(),
        _ => true,
    });
    let Some(caps) = caps else { return };
    for spec in specs.iter_mut() {
        let (field, names) = match spec.name.as_ref() {
            SET_EXPRESSION => ("expression", &caps.expressions),
            PLAY_ACTION => ("action", &caps.actions),
            _ => continue,
        };
        spec.input_schema["properties"][field]["enum"] = json!(names);
    }
}

fn avatar_turn() -> Result<Arc<AvatarTurn>> {
    match current_avatar_turn() {
        Some(turn) => Ok(turn),
        None => bail!("there is no avatar on this turn"),
    }
}

/// `input[field]`, if it is one of `declared`.
fn declared_name<'a>(input: &'a Value, field: &str, declared: &[String]) -> Result<&'a str> {
    let Some(name) = input[field].as_str() else {
        bail!("missing '{field}'");
    };
    if !declared.iter().any(|d| d == name) {
        bail!(
            "unknown {field} '{name}'; use one of: {}",
            declared.join(", ")
        );
    }
    Ok(name)
}

// -- set_expression ----------------------------------------------------------

static SET_EXPRESSION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: SET_EXPRESSION.into(),
    description: "Set your avatar's expression to match the feeling of your reply. \
                  Call it as often as you like within one turn."
        .into(),
    // The `enum` is filled per turn by `specialize_specs`.
    input_schema: json!({
        "type": "object",
        "properties": {
            "expression": { "type": "string", "enum": [] },
            "intensity": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "required": ["expression"]
    }),
});

pub(crate) struct SetExpressionTool;

#[async_trait]
impl Tool for SetExpressionTool {
    fn spec(&self) -> &ToolSpec {
        &SET_EXPRESSION_SPEC
    }

    /// `Think`: it changes nothing anywhere but the avatar's face, so it
    /// must run without a permission prompt on every origin.
    fn kind(&self) -> ToolKind {
        ToolKind::Think
    }

    async fn execute(&self, input: &Value) -> Result<String> {
        let turn = avatar_turn()?;
        let name = declared_name(input, "expression", &turn.caps.expressions)?;
        let intensity = match &input["intensity"] {
            Value::Null => 1.0,
            v => match v.as_f64() {
                Some(i) if (0.0..=1.0).contains(&i) => i as f32,
                _ => bail!("'intensity' must be a number from 0.0 to 1.0, got {v}"),
            },
        };
        turn.sink.expression(name, intensity).await;
        Ok("ok".into())
    }
}

// -- play_action -------------------------------------------------------------

static PLAY_ACTION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: PLAY_ACTION.into(),
    description: "Play a gesture once. Use it only at the moments that call for it, \
                  such as a greeting or a burst of joy."
        .into(),
    // The `enum` is filled per turn by `specialize_specs`.
    input_schema: json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": [] }
        },
        "required": ["action"]
    }),
});

pub(crate) struct PlayActionTool;

#[async_trait]
impl Tool for PlayActionTool {
    fn spec(&self) -> &ToolSpec {
        &PLAY_ACTION_SPEC
    }

    /// `Think`, as for `set_expression`.
    fn kind(&self) -> ToolKind {
        ToolKind::Think
    }

    async fn execute(&self, input: &Value) -> Result<String> {
        let turn = avatar_turn()?;
        // Validated before the gesture is spent, so a misspelt name does
        // not cost the turn its one action.
        let name = declared_name(input, "action", &turn.caps.actions)?;
        if turn.action_used.swap(true, Ordering::SeqCst) {
            bail!("an action was already played this turn; save gestures for moments that matter");
        }
        turn.sink.action(name).await;
        Ok("ok".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Records what reached the client, one line per notification.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<String>>);

    impl RecordingSink {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AvatarSink for RecordingSink {
        async fn expression(&self, name: &str, intensity: f32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("expression {name} {intensity}"));
        }

        async fn action(&self, name: &str) {
            self.0.lock().unwrap().push(format!("action {name}"));
        }
    }

    fn caps(expressions: &[&str], actions: &[&str]) -> AvatarCaps {
        AvatarCaps {
            expressions: expressions.iter().map(|s| s.to_string()).collect(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn test_turn(expressions: &[&str], actions: &[&str]) -> (Arc<AvatarTurn>, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let turn = AvatarTurn::new(caps(expressions, actions), sink.clone());
        (turn, sink)
    }

    fn names(specs: &[ToolSpec]) -> Vec<String> {
        specs.iter().map(|s| s.name.to_string()).collect()
    }

    #[tokio::test]
    async fn an_expression_in_the_list_is_sent_and_answers_ok() {
        let (turn, sink) = test_turn(&["neutral", "happy"], &["peace"]);
        let out = scope_avatar_turn(
            Some(turn),
            SetExpressionTool.execute(&json!({"expression":"happy","intensity":0.4})),
        )
        .await
        .unwrap();
        assert_eq!(out, "ok");
        assert_eq!(sink.events(), ["expression happy 0.4"]);
    }

    #[tokio::test]
    async fn an_unknown_expression_or_intensity_is_refused_without_a_notification() {
        let (turn, sink) = test_turn(&["neutral", "happy"], &[]);
        let err = scope_avatar_turn(
            Some(turn.clone()),
            SetExpressionTool.execute(&json!({"expression":"smug"})),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("neutral, happy"), "{err}");
        assert!(
            scope_avatar_turn(
                Some(turn),
                SetExpressionTool.execute(&json!({"expression":"happy","intensity":1.5}))
            )
            .await
            .is_err()
        );
        assert!(sink.events().is_empty());
    }

    #[tokio::test]
    async fn a_missing_intensity_is_full_strength() {
        let (turn, sink) = test_turn(&["happy"], &[]);
        scope_avatar_turn(
            Some(turn),
            SetExpressionTool.execute(&json!({"expression":"happy"})),
        )
        .await
        .unwrap();
        assert_eq!(sink.events(), ["expression happy 1"]);
    }

    #[tokio::test]
    async fn a_second_action_in_one_turn_is_refused() {
        let (turn, sink) = test_turn(&["neutral"], &["peace", "wave"]);
        scope_avatar_turn(
            Some(turn.clone()),
            PlayActionTool.execute(&json!({"action":"peace"})),
        )
        .await
        .unwrap();
        let err = scope_avatar_turn(
            Some(turn),
            PlayActionTool.execute(&json!({"action":"wave"})),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
        assert_eq!(sink.events(), ["action peace"]);
    }

    #[tokio::test]
    async fn an_unknown_action_does_not_use_up_the_turns_one_gesture() {
        let (turn, sink) = test_turn(&["neutral"], &["peace"]);
        assert!(
            scope_avatar_turn(
                Some(turn.clone()),
                PlayActionTool.execute(&json!({"action":"dab"}))
            )
            .await
            .is_err()
        );
        scope_avatar_turn(
            Some(turn),
            PlayActionTool.execute(&json!({"action":"peace"})),
        )
        .await
        .unwrap();
        assert_eq!(sink.events(), ["action peace"]);
    }

    #[tokio::test]
    async fn without_an_avatar_turn_the_tools_refuse() {
        assert!(
            SetExpressionTool
                .execute(&json!({"expression":"happy"}))
                .await
                .is_err()
        );
        assert!(
            scope_avatar_turn(None, PlayActionTool.execute(&json!({"action":"peace"})))
                .await
                .is_err()
        );
    }

    #[test]
    fn specs_follow_the_declaration() {
        let all = || {
            vec![
                SetExpressionTool.spec().clone(),
                PlayActionTool.spec().clone(),
            ]
        };
        let mut none = all();
        specialize_specs(&mut none, None);
        assert!(none.is_empty());

        let mut no_actions = all();
        specialize_specs(&mut no_actions, Some(&caps(&["neutral", "happy"], &[])));
        assert_eq!(names(&no_actions), ["set_expression"]);
        assert_eq!(
            no_actions[0].input_schema["properties"]["expression"]["enum"],
            json!(["neutral", "happy"])
        );

        let mut both = all();
        specialize_specs(&mut both, Some(&caps(&["neutral"], &["peace"])));
        assert_eq!(
            both[1].input_schema["properties"]["action"]["enum"],
            json!(["peace"])
        );

        let mut no_expressions = all();
        specialize_specs(&mut no_expressions, Some(&caps(&[], &["peace"])));
        assert_eq!(names(&no_expressions), ["play_action"]);
    }

    #[test]
    fn specialising_leaves_other_tools_alone() {
        let other = ToolSpec {
            name: "file_read".into(),
            description: "".into(),
            input_schema: json!({"type":"object"}),
        };
        let mut specs = vec![other.clone(), SetExpressionTool.spec().clone()];
        specialize_specs(&mut specs, None);
        assert_eq!(names(&specs), ["file_read"]);
        assert_eq!(specs[0].input_schema, other.input_schema);
    }
}
