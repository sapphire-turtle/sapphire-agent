use super::*;
use crate::config::{McpServerConfig, McpTransportConfig, McpTrust};
use crate::mcp_client::build_tools_for_client;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerConfig, Tool as RemoteTool,
};
use rmcp::service::{RequestContext, RoleServer, SubscriptionContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Pure parts
// ---------------------------------------------------------------------------

fn event(seq: u64, kind: EventKind, run_id: &str) -> TimerEvent {
    TimerEvent {
        seq,
        kind,
        run_id: run_id.to_string(),
        label: "Pomodoro".to_string(),
        ended_step: None,
        step: None,
    }
}

fn state(seq: u64, event: Option<TimerEvent>) -> TimerState {
    TimerState {
        seq,
        run: None,
        last_event: event,
    }
}

/// The shapes sapphire-timer serializes, read with fields the agent does
/// not use present and absent alike.
#[test]
fn the_servers_state_json_parses() {
    let json = r#"{
        "seq": 2,
        "run": {
            "id": "abc12", "label": "Pomodoro", "preset": "Pomodoro",
            "started_at": "2026-10-15T09:00:00+09:00",
            "cycle": 1, "total_cycles": 4, "step_index": 1, "total_steps": 2,
            "step": {"label": "Break", "minutes": 5.0},
            "step_started_at": "2026-10-15T09:25:00+09:00",
            "step_ends_at": "2026-10-15T09:30:00+09:00",
            "next_step": "Focus"
        },
        "last_event": {
            "seq": 2, "kind": "step_changed", "run_id": "abc12", "label": "Pomodoro",
            "at": "2026-10-15T09:25:00+09:00", "ended_step": "Focus", "step": "Break"
        }
    }"#;
    let state: TimerState = serde_json::from_str(json).expect("parse");
    let event = state.last_event.as_ref().expect("event");
    assert_eq!(event.kind, EventKind::StepChanged);
    assert_eq!(event.run_id, "abc12");
    let run = state.run.as_ref().expect("run");
    assert_eq!((run.cycle, run.total_cycles), (1, 4));

    let empty: TimerState = serde_json::from_str(r#"{"seq": 0}"#).expect("parse empty");
    assert_eq!(empty, TimerState::default());

    let future: TimerState = serde_json::from_str(
        r#"{"seq":1,"last_event":{"seq":1,"kind":"paused","run_id":"x","label":"T","at":"2026-10-15T09:00:00Z"}}"#,
    )
    .expect("an unknown kind still parses");
    assert_eq!(future.last_event.expect("event").kind, EventKind::Unknown);
}

#[test]
fn a_step_change_tells_the_user_to_switch_and_not_to_restart() {
    let mut e = event(2, EventKind::StepChanged, "r1");
    e.ended_step = Some("Focus".into());
    e.step = Some("Break".into());
    let run = TimerRun {
        id: "r1".into(),
        label: "Pomodoro".into(),
        cycle: 2,
        total_cycles: 4,
        step_index: 1,
        total_steps: 2,
        next_step: Some("Focus".into()),
    };
    let prompt = prompt_for(&e, Some(&run)).expect("a message");
    assert!(
        prompt.contains("'Focus' finished; 'Break' started"),
        "{prompt}"
    );
    assert!(prompt.contains("step 2/2 of cycle 2/4"), "{prompt}");
    assert!(prompt.contains("do NOT start another timer"), "{prompt}");

    // The run in the state belongs to someone else's later event: no
    // position rather than a wrong one.
    let other = TimerRun {
        id: "r2".into(),
        ..run
    };
    let prompt = prompt_for(&e, Some(&other)).expect("a message");
    assert!(!prompt.contains("cycle"), "{prompt}");
}

#[test]
fn a_finish_wraps_up_a_single_timer_and_a_preset_run_differently() {
    let mut single = event(2, EventKind::Finished, "r1");
    single.label = "Tea".into();
    single.ended_step = Some("Tea".into());
    let prompt = prompt_for(&single, None).expect("a message");
    assert!(prompt.starts_with("[Timer] 'Tea' elapsed"), "{prompt}");

    let mut preset = event(9, EventKind::Finished, "r1");
    preset.ended_step = Some("Break".into());
    let prompt = prompt_for(&preset, None).expect("a message");
    assert!(prompt.contains("final 'Break' step"), "{prompt}");
    assert!(prompt.contains("whole run is complete"), "{prompt}");
}

/// The user's own actions are answered by the turn that took them.
#[test]
fn starts_cancels_and_replacements_are_not_announced() {
    for kind in [
        EventKind::Started,
        EventKind::Cancelled,
        EventKind::Replaced,
        EventKind::Unknown,
    ] {
        assert_eq!(prompt_for(&event(1, kind, "r1"), None), None, "{kind:?}");
    }
}

#[test]
fn the_run_id_is_read_from_the_start_result() {
    let ok = CallToolResult::success(vec![ContentBlock::text(
        json!({"run": {"id": "r7", "label": "T"}, "resource": ACTIVE_TIMER_URI}).to_string(),
    )]);
    assert_eq!(started_run_id(&ok).as_deref(), Some("r7"));

    let structured = CallToolResult::structured(json!({"run": {"id": "r8"}}));
    assert_eq!(started_run_id(&structured).as_deref(), Some("r8"));

    let failed = CallToolResult::error(vec![ContentBlock::text(r#"{"run":{"id":"r9"}}"#)]);
    assert_eq!(
        started_run_id(&failed),
        None,
        "an error result started nothing"
    );

    let prose = CallToolResult::success(vec![ContentBlock::text("started")]);
    assert_eq!(started_run_id(&prose), None);
}

#[test]
fn only_the_newest_origins_are_kept() {
    let mut origins = Origins::default();
    for i in 0..ORIGINS_KEPT + 2 {
        origins.record(
            format!("r{i}"),
            TimerOrigin::Chat {
                room_id: format!("!{i}"),
            },
        );
    }
    assert!(origins.get("r0").is_none());
    assert!(origins.get("r1").is_none());
    assert_eq!(
        origins.get(&format!("r{}", ORIGINS_KEPT + 1)),
        Some(&TimerOrigin::Chat {
            room_id: format!("!{}", ORIGINS_KEPT + 1)
        })
    );
    origins.forget("r2");
    assert!(origins.get("r2").is_none());
}

#[test]
fn the_cursor_takes_the_first_read_as_history_and_counts_gaps() {
    let mut cursor = Cursor::default();
    assert_eq!(
        cursor.observe(&state(5, Some(event(5, EventKind::Finished, "old")))),
        Seen::Old,
        "what happened before the agent watched is not news"
    );
    assert_eq!(
        cursor.observe(&state(5, Some(event(5, EventKind::Finished, "old")))),
        Seen::Old
    );
    assert_eq!(
        cursor.observe(&state(6, Some(event(6, EventKind::Started, "r")))),
        Seen::New { missed: 0 }
    );
    assert_eq!(
        cursor.observe(&state(9, Some(event(9, EventKind::StepChanged, "r")))),
        Seen::New { missed: 2 }
    );
    // A restarted stdio server counts from 1 again.
    assert_eq!(
        cursor.observe(&state(1, Some(event(1, EventKind::Started, "s")))),
        Seen::New { missed: 0 }
    );

    let mut fresh = Cursor::default();
    assert_eq!(fresh.observe(&TimerState::default()), Seen::Old);
    assert_eq!(
        fresh.observe(&state(1, Some(event(1, EventKind::Started, "r")))),
        Seen::New { missed: 0 },
        "an empty slot is a baseline of 0"
    );
}

/// Records what would have been sent.
#[derive(Default)]
struct Recorder(Mutex<Vec<(String, String, TimerOrigin)>>);

impl TimerDispatch for Recorder {
    fn dispatch(&self, task_name: &str, prompt: &str, origin: &TimerOrigin) {
        self.0.lock().expect("recorder").push((
            task_name.to_string(),
            prompt.to_string(),
            origin.clone(),
        ));
    }
}

impl Recorder {
    fn sent(&self) -> Vec<(String, String, TimerOrigin)> {
        self.0.lock().expect("recorder").clone()
    }
}

#[test]
fn an_event_goes_to_the_origin_its_run_was_started_from() {
    let recorder = Arc::new(Recorder::default());
    let bridge = TimerBridge::new(Arc::clone(&recorder) as Arc<dyn TimerDispatch>);
    let voice = TimerOrigin::Voice {
        device_id: "kitchen".into(),
    };
    bridge
        .origins
        .lock()
        .unwrap()
        .record("r1".into(), voice.clone());

    bridge.handle_state(&TimerState::default());
    let mut step = event(2, EventKind::StepChanged, "r1");
    step.ended_step = Some("Focus".into());
    step.step = Some("Break".into());
    bridge.handle_state(&state(2, Some(step)));
    // Seen already: not sent twice.
    let mut again = event(2, EventKind::StepChanged, "r1");
    again.ended_step = Some("Focus".into());
    bridge.handle_state(&state(2, Some(again)));
    // A run nobody here started.
    bridge.handle_state(&state(3, Some(event(3, EventKind::Finished, "elsewhere"))));
    bridge.handle_state(&state(4, Some(event(4, EventKind::Finished, "r1"))));
    // Finished forgets the run.
    bridge.handle_state(&state(5, Some(event(5, EventKind::Finished, "r1"))));

    let sent = recorder.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent.iter()
            .all(|(task, _, origin)| task == "Pomodoro" && *origin == voice)
    );
}

#[test]
fn a_start_is_recorded_only_from_a_scoped_turn() {
    let bridge = TimerBridge::new(Arc::new(Recorder::default()) as Arc<dyn TimerDispatch>);
    let started = CallToolResult::success(vec![ContentBlock::text(
        json!({"run": {"id": "r1"}}).to_string(),
    )]);

    bridge.tool_called("timer_start", &started);
    assert!(bridge.origins.lock().unwrap().get("r1").is_none());

    let chat = TimerOrigin::Chat {
        room_id: "!room".into(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(scope_timer_origin(chat.clone(), async {
        bridge.tool_called("timer_status", &started);
        assert!(bridge.origins.lock().unwrap().get("r1").is_none());
        bridge.tool_called("timer_start_preset", &started);
    }));
    assert_eq!(bridge.origins.lock().unwrap().get("r1"), Some(&chat));
}

// ---------------------------------------------------------------------------
// End to end, against a server shaped like sapphire-timer
// ---------------------------------------------------------------------------

/// A stand-in for sapphire-timer: `timer_start` and the active resource,
/// with `subscriptions/listen` firing on every change of `state`.
#[derive(Clone)]
struct FakeTimer {
    state: Arc<tokio::sync::watch::Sender<String>>,
    listening: Arc<AtomicUsize>,
}

impl FakeTimer {
    fn new() -> Self {
        Self {
            state: Arc::new(tokio::sync::watch::channel(r#"{"seq":0}"#.to_string()).0),
            listening: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn set(&self, state: Value) {
        self.state.send_replace(state.to_string());
    }
}

impl ServerHandler for FakeTimer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let Value::Object(schema) = json!({"type": "object"}) else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![RemoteTool::new(
            "timer_start",
            "Start a timer",
            schema,
        )]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let run = json!({"id": "run1", "label": "Pomodoro", "cycle": 1, "total_cycles": 2,
                         "step_index": 0, "total_steps": 2});
        self.set(json!({"seq": 1, "run": run,
            "last_event": {"seq": 1, "kind": "started", "run_id": "run1", "label": "Pomodoro",
                           "at": "2026-10-15T09:00:00Z", "step": "Focus"}}));
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(json!({"run": run, "resource": ACTIVE_TIMER_URI}).to_string()),
        ])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let text = self.state.borrow().clone();
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(text, request.uri)],
        )))
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let mut changes = self.state.subscribe();
        changes.mark_unchanged();
        self.listening.fetch_add(1, Ordering::SeqCst);
        let sink = context.sink().clone();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err()
                        || sink.notify_resource_updated(ACTIVE_TIMER_URI).await.is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The whole path: a scoped `mcp__timer__timer_start` call records the
/// origin, the server's updates arrive over `subscriptions/listen`, and the
/// step change and finish — not the start — go to the room it came from.
#[tokio::test]
async fn timer_events_reach_the_room_the_timer_was_started_from() {
    let fake = FakeTimer::new();
    let server = fake.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, axum::Router::new().route_service("/mcp", service)).await;
    });

    let config = McpServerConfig {
        name: "timer".into(),
        trust: McpTrust::Edit,
        transport: McpTransportConfig::Http {
            url: format!("http://{addr}/mcp"),
            api_key: None,
        },
    };
    let client = Arc::new(McpClient::new(&config, "/tmp"));
    client.connect().await.expect("connect");
    let tools = build_tools_for_client(&client, client.list_tools().await.expect("tools"));

    let recorder = Arc::new(Recorder::default());
    TimerBridge::new(Arc::clone(&recorder) as Arc<dyn TimerDispatch>).spawn(Arc::clone(&client));
    eventually("the bridge subscribes", || {
        fake.listening.load(Ordering::SeqCst) > 0
    })
    .await;

    let room = TimerOrigin::Chat {
        room_id: "!study".into(),
    };
    let start = tools
        .iter()
        .find(|t| t.spec().name == "mcp__timer__timer_start")
        .expect("timer_start registered");
    scope_timer_origin(room.clone(), start.execute(&json!({"minutes": 25})))
        .await
        .expect("timer_start");

    fake.set(json!({"seq": 2,
        "run": {"id": "run1", "label": "Pomodoro", "cycle": 1, "total_cycles": 2,
                "step_index": 1, "total_steps": 2},
        "last_event": {"seq": 2, "kind": "step_changed", "run_id": "run1", "label": "Pomodoro",
                       "at": "2026-10-15T09:25:00Z", "ended_step": "Focus", "step": "Break"}}));
    eventually("the step change is announced", || {
        recorder.sent().len() == 1
    })
    .await;

    fake.set(json!({"seq": 3,
        "last_event": {"seq": 3, "kind": "finished", "run_id": "run1", "label": "Pomodoro",
                       "at": "2026-10-15T09:30:00Z", "ended_step": "Break"}}));
    eventually("the finish is announced", || recorder.sent().len() == 2).await;

    let sent = recorder.sent();
    assert!(
        sent.iter().all(|(_, _, origin)| *origin == room),
        "{sent:?}"
    );
    assert!(
        sent[0].1.contains("'Focus' finished; 'Break' started"),
        "{}",
        sent[0].1
    );
    assert!(sent[0].1.contains("step 2/2 of cycle 1/2"), "{}", sent[0].1);
    assert!(sent[1].1.contains("whole run is complete"), "{}", sent[1].1);
}
