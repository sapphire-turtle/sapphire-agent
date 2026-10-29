//! Timer notifications from a sapphire-timer MCP server.
//!
//! The timer itself is not the agent's any more: it runs in sapphire-timer,
//! which the agent reaches as an ordinary MCP server (`[timer] server` names
//! which of `[[tools.mcp_servers]]` it is). The LLM starts, cancels and
//! inspects it through that server's generic `mcp__<server>__timer_*` tools,
//! and presets live there too. What stays here is the part only the agent
//! can do — telling the user, on the channel they set the timer from, when
//! a step or the whole run ends:
//!
//! - **Origin capture.** The turn loops wrap every `tools.execute(...)` in
//!   [`scope_timer_origin`], so a tool can read where the turn came from with
//!   [`current_origin`]. When a `timer_start` / `timer_start_preset` call on
//!   the timer server succeeds, [`TimerBridge`] records the new run's id
//!   against that origin.
//! - **Watching.** [`TimerBridge::spawn`] listens on the server's
//!   `sapphire-timer://active` resource; each `notifications/resources/updated`
//!   is a cue to read the resource, and an event newer than the last one
//!   handled for a run with a recorded origin becomes a message — chat →
//!   `Agent::trigger`, voice → `serve::push_voice_text_to_subscriber`. No
//!   cross-channel fallback.
//!
//! sapphire-timer gives each MCP client one timer (per process over stdio,
//! per external device over HTTP), so there is one run at a time to follow.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rmcp::model::{CallToolResult, ServerNotification, SubscriptionFilter};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::mcp_client::{McpClient, ToolCallObserver, resource_text};

/// The resource holding the calling client's timer.
pub const ACTIVE_TIMER_URI: &str = "sapphire-timer://active";

/// How many runs' origins are remembered. The server runs one timer per
/// client, so anything but the newest is a run that has already ended; a few
/// cover the window where a replaced run's last update is still in flight.
const ORIGINS_KEPT: usize = 8;

/// Re-listen backoff after the subscription ends or cannot be opened.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Where the timer was set from. Single dispatch target, no fallback.
#[derive(Debug, Clone, PartialEq)]
pub enum TimerOrigin {
    /// Chat channel (Matrix / Discord). Fire path:
    /// `Agent::trigger(name, prompt, room_id)`.
    Chat { room_id: String },
    /// Voice satellite subscribed via `voice/subscribe`. Fire path:
    /// `serve::push_voice_text_to_subscriber(state, device_id, ...)`.
    Voice { device_id: String },
}

tokio::task_local! {
    static TIMER_ORIGIN_TL: TimerOrigin;
}

/// Run `fut` with `TIMER_ORIGIN_TL` set to `origin`. Called by the
/// agent / serve tool-execution loops per turn so a tool can read where
/// the originating message came from.
pub fn scope_timer_origin<F: std::future::Future>(
    origin: TimerOrigin,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TIMER_ORIGIN_TL.scope(origin, fut)
}

/// Read the active origin, when called inside `scope_timer_origin`.
/// Returns `None` from a turn that wasn't scoped (e.g. background catch-up).
pub fn current_origin() -> Option<TimerOrigin> {
    TIMER_ORIGIN_TL.try_with(|o| o.clone()).ok()
}

// ---------------------------------------------------------------------------
// The resource, as far as the agent reads it
// ---------------------------------------------------------------------------

/// sapphire-timer's `State`: the run in the slot and the last event.
/// Only the fields the messages use; unknown ones are ignored, so the
/// server can grow without breaking this.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct TimerState {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub run: Option<TimerRun>,
    #[serde(default)]
    pub last_event: Option<TimerEvent>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TimerRun {
    #[serde(deserialize_with = "id_string")]
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub cycle: u32,
    #[serde(default)]
    pub total_cycles: u32,
    /// 0-based, within the cycle.
    #[serde(default)]
    pub step_index: usize,
    #[serde(default)]
    pub total_steps: usize,
    #[serde(default)]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TimerEvent {
    pub seq: u64,
    pub kind: EventKind,
    #[serde(deserialize_with = "id_string")]
    pub run_id: String,
    pub label: String,
    #[serde(default)]
    pub ended_step: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Started,
    StepChanged,
    Finished,
    Cancelled,
    Replaced,
    /// A kind this build does not know. Never announced.
    #[serde(other)]
    Unknown,
}

/// Ids are grain ids, which serialize as strings; anything else is
/// rendered as its JSON so a format change degrades to "no match" rather
/// than a parse failure.
fn id_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => s,
        other => other.to_string(),
    })
}

/// The run id a successful `timer_start` / `timer_start_preset` returned:
/// its text is `{"run": Run, "resource": "..."}`.
pub fn started_run_id(result: &CallToolResult) -> Option<String> {
    if result.is_error == Some(true) {
        return None;
    }
    let from_value = |v: &Value| -> Option<String> {
        match v.get("run")?.get("id")? {
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    };
    if let Some(id) = result.structured_content.as_ref().and_then(from_value) {
        return Some(id);
    }
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .filter_map(|t| serde_json::from_str::<Value>(&t.text).ok())
        .find_map(|v| from_value(&v))
}

/// The message for `event`, or `None` when it is not worth one.
///
/// Only the timer's own transitions are announced. `started`, `replaced`
/// and `cancelled` follow from something the user just asked for, and the
/// turn that did it already answered.
pub fn prompt_for(event: &TimerEvent, run: Option<&TimerRun>) -> Option<String> {
    let ended = event.ended_step.as_deref().unwrap_or(&event.label);
    match event.kind {
        EventKind::StepChanged => {
            let next = event.step.as_deref().unwrap_or("");
            let position = run
                .filter(|r| r.id == event.run_id)
                .map(|r| {
                    format!(
                        " (step {}/{} of cycle {}/{})",
                        r.step_index + 1,
                        r.total_steps,
                        r.cycle,
                        r.total_cycles
                    )
                })
                .unwrap_or_default();
            Some(format!(
                "[Timer: {label}] '{ended}' finished; '{next}' started{position}. The next step \
                 is already running on the timer — do NOT start another timer. Just tell the \
                 user to switch. Keep it short.",
                label = event.label,
            ))
        }
        EventKind::Finished if ended == event.label => Some(format!(
            "[Timer] '{ended}' elapsed. Tell the user the timer is up. Keep it short."
        )),
        EventKind::Finished => Some(format!(
            "[Timer: {label}] The final '{ended}' step just ended and the whole run is \
             complete. Tell the user it is finished. Keep it short.",
            label = event.label,
        )),
        EventKind::Started | EventKind::Cancelled | EventKind::Replaced | EventKind::Unknown => {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bookkeeping
// ---------------------------------------------------------------------------

/// Run id → origin, newest last, at most [`ORIGINS_KEPT`].
#[derive(Debug, Default)]
struct Origins(VecDeque<(String, TimerOrigin)>);

impl Origins {
    fn record(&mut self, run_id: String, origin: TimerOrigin) {
        self.0.retain(|(id, _)| *id != run_id);
        self.0.push_back((run_id, origin));
        while self.0.len() > ORIGINS_KEPT {
            self.0.pop_front();
        }
    }

    fn get(&self, run_id: &str) -> Option<&TimerOrigin> {
        self.0.iter().find(|(id, _)| id == run_id).map(|(_, o)| o)
    }

    fn forget(&mut self, run_id: &str) {
        self.0.retain(|(id, _)| id != run_id);
    }
}

/// What a newly read state means for the last handled sequence number.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Nothing new.
    Old,
    /// A new event; `missed` updates were coalesced on the way.
    New { missed: u64 },
}

/// Tracks the last event handled, across subscriptions.
#[derive(Debug, Default)]
struct Cursor {
    /// `None` until the first read: whatever happened before the agent
    /// started watching is history, not news.
    last: Option<u64>,
}

impl Cursor {
    fn observe(&mut self, state: &TimerState) -> Seen {
        let Some(event) = &state.last_event else {
            // An empty slot that never ran. Still a baseline.
            self.last.get_or_insert(state.seq);
            return Seen::Old;
        };
        let Some(last) = self.last else {
            self.last = Some(event.seq);
            return Seen::Old;
        };
        if event.seq < last {
            // The counter went backwards: a stdio server restarted and
            // started counting again. Everything it says now is new.
            info!(
                "Timer: event sequence went back from {last} to {}; the timer server restarted",
                event.seq
            );
            self.last = Some(event.seq);
            return Seen::New {
                missed: event.seq.saturating_sub(1),
            };
        }
        if event.seq == last {
            return Seen::Old;
        }
        self.last = Some(event.seq);
        Seen::New {
            missed: event.seq - last - 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Where a timer message goes. A trait so tests can see what would be sent
/// without an agent or a voice satellite.
pub trait TimerDispatch: Send + Sync {
    fn dispatch(&self, task_name: &str, prompt: &str, origin: &TimerOrigin);
}

/// The production dispatcher: chat through `Agent::trigger`, voice through
/// `serve::push_voice_text_to_subscriber`. Both refs are installed by `serve`
/// after the agent and `ServeState` exist (weak, to keep the graph acyclic).
#[derive(Default)]
pub struct ChannelDispatch {
    agent: tokio::sync::OnceCell<Weak<crate::agent::Agent>>,
    serve_state: tokio::sync::OnceCell<Weak<crate::serve::ServeState>>,
}

impl ChannelDispatch {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_agent(&self, agent: Weak<crate::agent::Agent>) {
        let _ = self.agent.set(agent);
    }

    pub fn set_serve_state(&self, state: Weak<crate::serve::ServeState>) {
        let _ = self.serve_state.set(state);
    }
}

impl TimerDispatch for ChannelDispatch {
    /// Send the message to the right channel. Logs and drops on failure —
    /// no fallback to the other channel.
    ///
    /// The dispatch is detached via `tokio::spawn` so the trigger does not
    /// run inside the bridge's watch task. The LLM may well start a new
    /// timer in the turn this message triggers, and that tool call must
    /// not wait on — or be torn down with — the task that is reading the
    /// timer resource: a turn cut short mid-tool-call leaves a `tool_use`
    /// without its `tool_result`, and the next turn then fails with
    /// Anthropic 400 `tool_use ids were found without tool_result blocks
    /// immediately after`.
    fn dispatch(&self, task_name: &str, prompt: &str, origin: &TimerOrigin) {
        match origin {
            TimerOrigin::Chat { room_id } => match self.agent.get().and_then(Weak::upgrade) {
                Some(agent) => {
                    let task_name = task_name.to_string();
                    let prompt = prompt.to_string();
                    let room_id = room_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.trigger(&task_name, &prompt, &room_id).await {
                            warn!("Timer fire (chat) failed for {task_name} -> {room_id}: {e:#}");
                        }
                    });
                }
                None => {
                    warn!("Timer fire (chat) skipped for {task_name}: no agent to trigger")
                }
            },
            TimerOrigin::Voice { device_id } => {
                match self.serve_state.get().and_then(Weak::upgrade) {
                    Some(state) => {
                        let task_name = task_name.to_string();
                        let prompt = prompt.to_string();
                        let device_id = device_id.clone();
                        tokio::spawn(async move {
                            if let Err(e) = crate::serve::push_voice_text_to_subscriber(
                                state,
                                device_id.clone(),
                                Some(task_name.clone()),
                                prompt,
                            )
                            .await
                            {
                                let reason = match e {
                                    crate::serve::VoicePushError::Offline => "offline".to_string(),
                                    crate::serve::VoicePushError::NoVoice => {
                                        "no voice providers".to_string()
                                    }
                                    crate::serve::VoicePushError::NotConfigured => {
                                        "voice_pipeline not configured for room_profile".to_string()
                                    }
                                    crate::serve::VoicePushError::Other(msg) => msg,
                                };
                                warn!(
                                    "Timer fire (voice) failed for {task_name} -> device={device_id}: {reason}"
                                );
                            }
                        });
                    }
                    None => warn!(
                        "Timer fire (voice) skipped for {task_name}: serve state not wired in"
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------

/// Connects the timer server's events to the channels timers were set from.
pub struct TimerBridge {
    origins: Mutex<Origins>,
    cursor: Mutex<Cursor>,
    dispatch: Arc<dyn TimerDispatch>,
}

impl TimerBridge {
    pub fn new(dispatch: Arc<dyn TimerDispatch>) -> Arc<Self> {
        Arc::new(Self {
            origins: Mutex::new(Origins::default()),
            cursor: Mutex::new(Cursor::default()),
            dispatch,
        })
    }

    /// Hook the bridge into `client` and start watching its timer. Runs for
    /// the life of the process.
    pub fn spawn(self: &Arc<Self>, client: Arc<McpClient>) {
        client.set_observer(Arc::clone(self) as Arc<dyn ToolCallObserver>);
        let me = Arc::clone(self);
        tokio::spawn(async move { me.watch(client).await });
    }

    /// Listen, reconcile, handle updates; on any end, back off and listen
    /// again. Subscription streams are not resumable, so every new one
    /// starts by reading the resource once.
    async fn watch(self: Arc<Self>, client: Arc<McpClient>) {
        let filter = SubscriptionFilter::builder()
            .resource_subscription(ACTIVE_TIMER_URI)
            .build();
        let mut backoff = BACKOFF_MIN;
        loop {
            match client.listen(filter.clone()).await {
                Ok(mut subscription) => {
                    info!("Timer: watching '{}' on {ACTIVE_TIMER_URI}", client.name());
                    backoff = BACKOFF_MIN;
                    self.refresh(&client).await;
                    loop {
                        match subscription.next().await {
                            Ok(Some(ServerNotification::ResourceUpdatedNotification(n)))
                                if n.params.uri == ACTIVE_TIMER_URI =>
                            {
                                self.refresh(&client).await;
                            }
                            Ok(Some(other)) => debug!("Timer: notification: {other:?}"),
                            Ok(None) => {
                                info!(
                                    "Timer: subscription to '{}' ended ({:?}); listening again",
                                    client.name(),
                                    subscription.end()
                                );
                                break;
                            }
                            Err(e) => {
                                warn!(
                                    "Timer: subscription to '{}' failed: {e}; listening again",
                                    client.name()
                                );
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(
                    "Timer: cannot watch '{}': {e:#}; retrying in {}s",
                    client.name(),
                    backoff.as_secs()
                ),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Read the resource and handle what changed.
    async fn refresh(&self, client: &McpClient) {
        let result = match client.read_resource(ACTIVE_TIMER_URI).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Timer: reading {ACTIVE_TIMER_URI} failed: {e:#}");
                return;
            }
        };
        match serde_json::from_str::<TimerState>(&resource_text(&result)) {
            Ok(state) => self.handle_state(&state),
            Err(e) => warn!("Timer: {ACTIVE_TIMER_URI} is not a timer state: {e}"),
        }
    }

    /// Act on one read of the resource.
    fn handle_state(&self, state: &TimerState) {
        let seen = self.cursor.lock().expect("timer cursor").observe(state);
        let Seen::New { missed } = seen else {
            return;
        };
        let Some(event) = &state.last_event else {
            return;
        };
        if missed > 0 {
            info!(
                "Timer: {missed} update(s) before event {} were coalesced; only the latest is handled",
                event.seq
            );
        }
        let origin = {
            let mut origins = self.origins.lock().expect("timer origins");
            let origin = origins.get(&event.run_id).cloned();
            if matches!(
                event.kind,
                EventKind::Finished | EventKind::Cancelled | EventKind::Replaced
            ) {
                origins.forget(&event.run_id);
            }
            origin
        };
        let Some(origin) = origin else {
            debug!(
                "Timer: {:?} for run {} has no recorded origin; not announced",
                event.kind, event.run_id
            );
            return;
        };
        if let Some(prompt) = prompt_for(event, state.run.as_ref()) {
            self.dispatch.dispatch(&event.label, &prompt, &origin);
        }
    }
}

impl ToolCallObserver for TimerBridge {
    /// Runs inside the turn's `scope_timer_origin`, which is what makes
    /// [`current_origin`] the origin of the turn that started the timer.
    fn tool_called(&self, tool: &str, result: &CallToolResult) {
        if !matches!(tool, "timer_start" | "timer_start_preset") {
            return;
        }
        let Some(origin) = current_origin() else {
            return;
        };
        match started_run_id(result) {
            Some(run_id) => {
                debug!("Timer: run {run_id} started from {origin:?}");
                self.origins
                    .lock()
                    .expect("timer origins")
                    .record(run_id, origin);
            }
            None => warn!("Timer: {tool} succeeded but its result names no run id"),
        }
    }
}

#[cfg(test)]
mod tests;
