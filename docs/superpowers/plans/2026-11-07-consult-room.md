# Consult Room Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let autonomous sessions ask a person a question in a configured chat room (Discord or Matrix) with `ask_human`, matched by reply, and receive the answer, a timeout, or follow-ups through the background-task inbox.

**Architecture:** Channels gain three platform capabilities (posted IDs, incoming `reply_to`, outgoing `reply_to`). A question is a `TaskKind::Question` in `BackgroundTasks`. A new platform-neutral `consult.rs` owns posting, routing replies, deadlines and room notes; the `ask_human` tool and `Agent::run` call into it.

**Tech Stack:** Rust, tokio, serde_json, serenity 0.12, matrix-sdk 0.19 (ruma), chrono.

**Spec:** `docs/superpowers/specs/2026-11-07-consult-room-design.md` (builds on `docs/superpowers/specs/2026-11-06-background-tasks-design.md`)

## Global Constraints

- All paths below are under `crates/sapphire-agent-server/` unless they start with `docs/`.
- Platform-specific code lives only in `channel/discord.rs` and `channel/matrix.rs`; `consult.rs`, `background.rs` and the tool never name a platform.
- Open questions per session: at most `3` (constant `MAX_OPEN_QUESTIONS`). Questions don't count toward `[tools.background] max_per_session`.
- Config: `[autonomous] consult_room: Option<String>` (no default), `consult_timeout_hours: u64` (default `24`). `AutonomousConfig` keeps `deny_unknown_fields`.
- Exact strings (copy verbatim):
  - Post: `[Autonomous: {task}]` then, only for a child, ` question from subagent {handle}`; newline; the question; newline; `(Reply to this message to answer. Waiting until {deadline}.)` with `{deadline}` as local time `%Y-%m-%d %H:%M`.
  - Tool success (session): `Question {id} posted. The answer will be delivered to this conversation when it arrives; keep working or reply WAIT.`
  - Tool success (child): `Question {id} posted. The answer goes to the session that started you, not to you; finish what you can and mention the question in your report.`
  - Cap refusal: `You already have 3 questions waiting for an answer. Wait for one, or withdraw one with background_task.`
  - Not served: `consult room {room} is not served on this host`
  - No replies: `consult room {room} is on a channel that cannot match replies`
  - Not autonomous: `ask_human is only available in autonomous sessions.`
  - Room notes: `That question has ended (withdrawn, or its session finished).` / `Reply to a question to answer it.` / `No answer by the deadline; the session will decide on its own. A reply now still reaches it as extra information.` / `This question was withdrawn.`
  - Image note: `(an image was attached but not forwarded)`
  - Render headers: `[Question {id} answered by {from}: "{first line}"]`, `[Question {id}: no answer by the deadline: "{first line}"]` followed by the line `Decide for yourself whether to proceed on a guess or stop; if you proceed, say so in your report.`, `[Question {id} follow-up by {from}: "{first line}"]`, `[Question {id} was withdrawn: "{first line}"]`, `[Question {id} was interrupted by a restart: "{first line}"]`; with `asked_by`, a last line `To pass this to the subagent, resume handle {h}.`
- Commit scopes: `feat(channel)`, `feat(discord)`, `feat(matrix)`, `feat(tools)`, `feat(autonomous)`. No `cliff.toml` change.
- Each task ends with `cargo fmt --all`, `cargo clippy -p sapphire-agent-server --all-targets -- -D warnings` and `cargo test -p sapphire-agent-server` green.

**Spec amendments (already applied to the spec):** the no-answer header reads `no answer by the deadline` instead of `no answer within 24h`, because `render` doesn't know the configured timeout. The spec's `TurnContext.autonomous_task` is carried as `TurnHost::consult_identity()` instead (see Task 5): the autonomous host already identifies the turn, and it survives `detached()` into background children without a new `TurnContext` field at all seven construction sites.

## Review Focus

1. A long question that the channel splits into several messages: a reply to any part (not just the first) must be the answer. → Task 4 test `reply_to_a_later_part_answers`.
2. A deadline timer that fires after the session was torn down (DONE) or the question was answered must do nothing: no note, no completion. → Task 4 test `a_late_timer_after_teardown_is_silent`.
3. Restart with a deadline already past: the question expires once, right away, with one deadline note. → Task 4 test `rearm_expires_past_deadlines_once`.
4. A Matrix thread message whose `in_reply_to` is only the thread fallback (`is_falling_back = true`) is not a reply. → Task 2 test `thread_fallback_is_not_a_reply`.
5. Messages in rooms other than the consult room still reach normal chat, and two sessions' questions route to their own sessions. → Task 4 test `routes_to_the_asking_session`, Task 6 test `other_rooms_still_reach_chat`.

---

### Task 1: Channel capabilities in the trait, and Discord

**Files:**
- Modify: `src/channel/mod.rs` (`IncomingMessage`, `OutgoingMessage`, `Channel`, `Channels`)
- Modify: `src/channel/discord.rs`
- Modify: `src/channel/matrix.rs` (only to compile: return `Ok(vec![])` from `send`, fill `reply_to: None`; Task 2 implements it)
- Modify: `src/agent.rs` (callers and the test `RecordingChannel`), any other `IncomingMessage {` / `OutgoingMessage {` literal

**Interfaces:**
- Produces:
  - `IncomingMessage.reply_to: Option<String>`; `OutgoingMessage.reply_to: Option<String>` (`OutgoingMessage::new` sets `None`; add `fn replying_to(self, id: impl Into<String>) -> Self`).
  - `Channel::send(&self, &OutgoingMessage) -> anyhow::Result<Vec<String>>` — the posted message IDs in order.
  - `Channel::supports_replies(&self) -> bool` (default `false`).
  - `Channels::send(&self, &OutgoingMessage) -> Result<Vec<String>>`; `Channels::supports_replies(&self, room_id: &str) -> Option<bool>` (`None` when no channel serves the room — no "first channel" fallback here); `Channels::serves(&self, room_id: &str) -> bool`.
  - `discord::reply_target(msg: &serenity::all::Message) -> Option<String>` (pure).

- [ ] **Step 1: Tests (in `channel/discord.rs` tests)**
  - `reply_target_reads_message_reference`: a `Message` deserialized from JSON with `"message_reference": {"message_id": "42", "channel_id": "7"}` → `Some("42")`; without the field → `None`.
  - In `channel/mod.rs` tests, `channels_report_reply_support_per_room`: a `Channels` with one fake channel `"fake"` (`supports_replies = true`) seeded `{"r1": "fake"}` → `supports_replies("r1") == Some(true)`, `supports_replies("r2") == None`, `serves("r1")`, `!serves("r2")`; `send` returns the fake's IDs.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server channel::` — expect compile failures (missing fields/methods).
- [ ] **Step 3: Implement.** Discord `send`: build each chunk with `CreateMessage::new().content(chunk)`, add `.reference_message((channel_id, MessageId::new(id)))` to the first chunk when `reply_to` parses as `u64` (a non-numeric `reply_to` is an error: `Discord reply_to is not a message ID`), send with `channel_id.send_message`, collect `msg.id.to_string()`. Listener sets `reply_to: reply_target(&msg)`. `supports_replies` → `true`. Update every literal and caller; `agent.rs` callers ignore the IDs (`.map(|_| ())` or `let _ =`).
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS. fmt + clippy clean.
- [ ] **Step 5: Commit** `feat(channel): return posted message IDs and carry reply targets` (Discord in the same commit is fine, or a second `feat(discord): …`).

### Task 2: Matrix replies

**Files:**
- Modify: `src/channel/matrix.rs`

**Interfaces:**
- Consumes: Task 1's trait.
- Produces: `matrix::reply_target(rel: Option<&Relation<RoomMessageEventContentWithoutRelation>>) -> Option<String>` (use whatever relation type `event.content.relates_to` has in matrix-sdk 0.19); `matrix::strip_reply_fallback(body: &str) -> String`.

- [ ] **Step 1: Tests**
  - `reply_relation_is_a_reply`: `Relation::Reply { in_reply_to: InReplyTo::new(eid) }` → `Some(eid)`.
  - `thread_reply_is_a_reply`: `Relation::Thread` with `in_reply_to = Some(e2)`, `is_falling_back = false` → `Some(e2)`.
  - `thread_fallback_is_not_a_reply`: same with `is_falling_back = true` → `None`.
  - `no_relation_is_not_a_reply` → `None`.
  - `strip_reply_fallback`: `"> <@a:x> q\n> more\n\nanswer"` → `"answer"`; `"answer"` → `"answer"`; `"> quoted only"` → `""`; `"line\n> not leading"` unchanged.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server channel::matrix` — FAIL.
- [ ] **Step 3: Implement.** Listener: `reply_to = reply_target(event.content.relates_to.as_ref())`; when `Some`, the text body goes through `strip_reply_fallback`. `send`: with `reply_to`, set `content.relates_to = Some(Relation::Reply { in_reply_to: InReplyTo::new(id) })` (when both `thread_id` and `reply_to` are set, the reply wins); return `vec![response.event_id.to_string()]`. `supports_replies` → `true`.
- [ ] **Step 4: Run** tests — PASS; fmt + clippy clean.
- [ ] **Step 5: Commit** `feat(matrix): post and receive replies`.

### Task 3: Questions in the background layer

**Files:**
- Modify: `src/background.rs`
- Modify: `src/tools/background_tools.rs` (adapt to `cancel`'s new return; list shows a question as `question "{first line}"`)
- Modify: `src/serve/mod.rs` (`place_completions` keeps calling `render`; `resumable` returns `false` for questions)

**Interfaces:**
- Produces:
  - `TaskKind::Question { room: String, message_ids: Vec<String>, question: String, asked_by: Option<String>, deadline: DateTime<Utc> }`.
  - `Outcome::Answered { text: String, from: String }`, `Outcome::NoAnswer`, `Outcome::FollowUp { text: String, from: String }`.
  - `pub(crate) enum Route { Pending { session: String, id: TaskId }, Answered { session: String, id: TaskId }, Unknown }`.
  - `pub(crate) const MAX_OPEN_QUESTIONS: usize = 3;`
  - `BackgroundTasks::register_question(&self, session: &str, kind: TaskKind, label: String) -> Result<TaskId>` — enforces `MAX_OPEN_QUESTIONS` (error text is the Global Constraints cap refusal) and not `max_per_session`. `register` stays for subagents only (`debug_assert!` that the kind isn't a question).
  - `route_reply(&self, room: &str, reply_to: &str) -> Route`
  - `answer(&self, session: &str, id: &str, text: String, from: String) -> bool`
  - `follow_up(&self, session: &str, id: &str, text: String, from: String) -> bool` (false if `id` isn't a closed question of `session`)
  - `expire(&self, session: &str, id: &str) -> bool`
  - `open_questions(&self, session: &str) -> usize`
  - `pending_questions(&self) -> Vec<(String, PendingTask)>` (every session; for re-arming)
  - `cancel(&self, session: &str, id: &str) -> Result<PendingTask>` (was `Result<()>`; returns what it cancelled)
  - `cancel_all(&self, session: &str) -> Vec<PendingTask>` — the pending **questions** it dropped (subagents are aborted as before and not returned).
  - `render` handles every `(Question, outcome)` pair with the Global Constraints headers; `Cancelled` → withdrawn header, `Interrupted` → interrupted header (a question is only interrupted if an old file is loaded by an older binary; keep it total).

- [ ] **Step 1: Tests (in `background.rs` tests)**
  - `question_answer_moves_it_to_closed`: register → `route_reply(room, mid)` is `Pending`; `answer` → true, inbox has `Answered`; `route_reply` is now `Answered`; `open_questions == 0`.
  - `follow_up_queues_without_pending`: after answer, `follow_up` → true, a `FollowUp` completion in the inbox, `pending` empty.
  - `expire_only_once`: `expire` → true then false; `answer` after expire → false; `route_reply` → `Answered`.
  - `fourth_question_is_refused`: 3 `register_question` ok, 4th errs with the exact cap text; 4 subagents still registrable up to `max_per_session` independently.
  - `open_keeps_questions_pending`: persist to a temp dir, reopen → the question is still pending (not `Interrupted`), a subagent becomes `Interrupted`; `closed_questions` survive the reopen.
  - `cancel_all_returns_questions_and_forgets`: returns the one pending question; afterwards `route_reply` for both a pending and a closed question → `Unknown`.
  - `render_question_outcomes`: exact strings for answered, no-answer (two lines), follow-up, withdrawn, and the `asked_by` resume line; the first line of a multi-line question is used.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server background::` — FAIL.
- [ ] **Step 3: Implement.** `SessionTasks` gains `closed_questions: Vec<PendingTask>`; `Persisted` gains it with `#[serde(default)]`. `route_reply` scans pending then closed across sessions, matching `room` and membership in `message_ids`. `has_pending_or_inbox` already counts pending tasks — confirm questions are in `pending`.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS; fmt + clippy.
- [ ] **Step 5: Commit** `feat(tools): questions as background tasks`.

### Task 4: The consult core (`consult.rs`) and its config

**Files:**
- Create: `src/consult.rs` (add `mod consult;` in `lib.rs`)
- Modify: `src/config.rs` (`AutonomousConfig`)

**Interfaces:**
- Consumes: Task 1 `Channels`, Task 3 layer API.
- Produces:
  - `AutonomousConfig.consult_room: Option<String>`, `AutonomousConfig.consult_timeout_hours: u64` (default 24).
  - `pub(crate) struct Consult` with:
    - `Consult::new(channels: Arc<Channels>, background: Arc<BackgroundTasks>, room: String, timeout: std::time::Duration) -> Arc<Self>`
    - `async fn ask(self: &Arc<Self>, owner_session: &str, task: &str, asked_by: Option<&str>, question: &str) -> anyhow::Result<TaskId>` — checks `serves` / `supports_replies` (errors are the exact Global Constraints texts), cap (via `register_question`, but check `open_questions` **before** posting so a refused ask posts nothing), posts, registers, spawns the deadline timer.
    - `fn is_consult_room(&self, room_id: &str) -> bool`
    - `async fn handle_incoming(&self, msg: &IncomingMessage)` — the routing table from the spec; text plus the image note.
    - `async fn withdraw(&self, questions: Vec<PendingTask>)` — posts `This question was withdrawn.` as a reply to each question's first message ID.
    - `fn rearm(self: &Arc<Self>)` — spawns a timer for every `pending_questions()` entry (past deadlines fire at once).
  - Timer: `tokio::spawn` sleeping `(deadline - Utc::now()).to_std().unwrap_or_default()`, then `if background.expire(..) { post deadline note as a reply }`.
  - Room notes reply to the incoming message (`reply_to = msg.id`); the deadline and withdrawal notes reply to the question's first message ID. A failed note send is `warn!` only.

- [ ] **Step 1: Tests (in `consult.rs`, with a fake `Channel` that records `OutgoingMessage`s, hands out IDs `m1, m2, …`, splits a message into two posts when it contains `"<split>"`, and reports `supports_replies = true`; `Channels` seeded `{"room": "fake"}`; `BackgroundTasks::in_memory(4)`)**
  - `ask_posts_and_registers`: post text matches the exact format (assert prefix `[Autonomous: t]\nwhy?\n(Reply to this message to answer. Waiting until `), one pending question with `message_ids == ["m1"]`.
  - `child_ask_names_the_subagent`: `asked_by = Some("h-1")` → first line `[Autonomous: t] question from subagent h-1`.
  - `reply_answers_and_bot_stays_silent`: incoming `reply_to = "m1"` → `Answered` completion with `from = sender`; no new post.
  - `reply_to_a_later_part_answers`: question containing `<split>` → `message_ids == ["m1","m2"]`; reply to `m2` answers.
  - `second_reply_is_a_follow_up`.
  - `unknown_reply_and_non_reply_get_notes`: exact note texts, each a reply to the incoming message's ID.
  - `image_only_reply_delivers_the_note`: empty text + one attachment → answer text is exactly `(an image was attached but not forwarded)`; text + image → text, newline, note.
  - `routes_to_the_asking_session`: sessions A and B each ask; replies land in their own inboxes.
  - `deadline_expires_and_notes`: `timeout = Duration::from_millis(10)`; after ~50ms, inbox has `NoAnswer`, the deadline note was posted as a reply to `m1`.
  - `a_late_timer_after_teardown_is_silent`: timeout 30ms; `cancel_all` then `withdraw` at once; after 80ms only the withdrawal note was posted, inbox empty for the session.
  - `rearm_expires_past_deadlines_once`: register a question directly (via `register_question`) with a past `deadline`, call `rearm()` once, let it run; exactly one `NoAnswer` and one deadline note.
  - `refusals_post_nothing`: room not served → exact not-served error; channel without replies → exact no-replies error; 4th question → cap error; the fake recorded no post for any of them.
  - `config_parses_consult_fields`: TOML `[autonomous]\nconsult_room = "r"\n` → `Some("r")`, timeout 24; unknown key still rejected.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server consult::` — FAIL.
- [ ] **Step 3: Implement** per the interfaces.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS; fmt + clippy.
- [ ] **Step 5: Commit** `feat(tools): a consult room that matches questions to replies`.

### Task 5: `ask_human` and who may call it

**Files:**
- Create: `src/tools/ask_human.rs` (add to `tools/mod.rs`)
- Modify: `src/serve/mod.rs` (`TurnHost::consult_identity`, `DetachedHost` carries it, `visible_tool_predicate` hides `ask_human`, `run_llm_turn` lets it back in, `ServeState.consult: std::sync::OnceLock<Arc<Consult>>` + `fn consult(&self) -> Option<&Arc<Consult>>` + `fn set_consult(&self, Arc<Consult>)`)
- Modify: `src/autonomous.rs` (`AutonomousHost` gets `task: String`, `session: String`)
- Modify: `src/tools/subagent.rs` (`BackgroundSubagentHost` gets the child's `handle`; `consult_identity` forwards the parent's with `asked_by = Some(handle)`)
- Modify: `src/tools/background_tools.rs` (cancelling a question calls `consult.withdraw(vec![task])` when a consult exists)

**Interfaces:**
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub(crate) struct ConsultIdentity { pub task: String, pub owner_session: String, pub asked_by: Option<String> }`
  - `TurnHost::consult_identity(&self) -> Option<ConsultIdentity>` (default `None`). The default `detached()` copies it into `DetachedHost`.
  - `AskHumanTool` — name `ask_human`, kind `Think`, schema `{ "question": string (required) }`, description: `Ask the person overseeing autonomous work a question in the consult room. Returns at once; the answer arrives later in this conversation. Use it for unclear requirements you cannot settle from the code or docs.`
  - Behaviour: no turn context or `consult_identity() == None` → error `ask_human is only available in autonomous sessions.`; `state.consult()` is `None` → the not-served error with the configured room; else `consult.ask(owner_session, task, asked_by, question)` and the exact success text (child variant when `asked_by` is `Some`).

- [ ] **Step 1: Tests**
  - `serve/mod.rs`: `ask_human_is_hidden_without_consult_identity` — `visible_tool_predicate(..)("ask_human") == false` for every flag combination; `run_llm_turn`'s filter includes it only for a host whose `consult_identity()` is `Some` (test through the spec-filter closure if it's extracted, otherwise a scripted turn that records the specs the provider saw).
  - `detached_keeps_consult_identity`: a host with an identity → `detached().consult_identity()` equal.
  - `tools/ask_human.rs`: refuses without identity (exact text); refuses with the not-served text when `ServeState` has no consult; with a consult over the fake channel (reuse Task 4's fake via a `#[cfg(test)] pub(crate)` helper in `consult.rs`) returns the exact session success text and one post; a child identity returns the child text and the post names the subagent.
  - `tools/subagent.rs`: `background_child_forwards_identity_with_its_handle`.
  - `tools/background_tools.rs`: `cancelling_a_question_posts_the_withdrawal`.
- [ ] **Step 2: Run** the new tests — FAIL.
- [ ] **Step 3: Implement.** Register `AskHumanTool` in `lib.rs` only when `config.autonomous.consult_room.is_some()`.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS; fmt + clippy.
- [ ] **Step 5: Commit** `feat(tools): ask_human for autonomous sessions`.

### Task 6: Wiring — receiving, teardown, startup, verify, docs

**Files:**
- Modify: `src/agent.rs` (`Agent.consult: std::sync::OnceLock<Arc<Consult>>`, `fn set_consult`; in `run`, a message with `consult.is_consult_room(&room_id)` goes to `consult.handle_incoming` in a spawned task and never to `handle_message`)
- Modify: `src/autonomous.rs` (where it calls `cancel_all` before closing: pass the returned questions to `state.consult()`'s `withdraw`)
- Modify: `src/tools/config_tools.rs` (its `cancel_all` caller ignores the return)
- Modify: `src/lib.rs` (in the channel block, after `Channels` and `Agent` are built: if `consult_room` is set, build `Consult::new(channels, serve_state.background.clone(), room, hours)`, `serve_state.set_consult`, `agent.set_consult`, `consult.rearm()`; if `consult_room` is set but the channel block doesn't run or `!channels.serves(room)`, `warn!("consult room {room} is not served on this host; ask_human will refuse")`. In `verify`: warn when `consult_room` is in neither `discord.channel_ids` nor `matrix.room_ids`, and when `autonomous.enabled` is false.)
- Modify: `config.example.toml` (commented `consult_room` and `consult_timeout_hours` under `[autonomous]`, with one line saying replies match answers, and that the room must also be in the channel's room list)

**Interfaces:**
- Consumes: Tasks 1–5.

- [ ] **Step 1: Tests**
  - `agent.rs`: `consult_room_messages_skip_chat` — an agent with a consult on room `"consult"`: injecting a message there produces no chat session and no history entry, and the consult saw it (e.g. a non-reply gets the `Reply to a question to answer it.` note on the recording channel). `other_rooms_still_reach_chat` — a message in another room is handled as chat as before.
  - `autonomous.rs` (register `AskHumanTool` on the test state's `tools`, and `set_consult` a consult over the fake channel): `a_question_answer_resumes_the_session` — scripted provider (a `ChatResponse` with a `tool_calls` entry for `ask_human`): turn 1 calls `ask_human` (tool call) then replies `WAIT`; the test injects the answer through `consult.handle_incoming` with `reply_to` = the posted ID; next `run_cycle` resumes the same session and its user message contains `[Question … answered by …`. `done_withdraws_open_questions` — a session that asks then says `DONE` posts `This question was withdrawn.` and the question routes to `Unknown`.
  - `lib.rs` / config tests: `config.example.toml` still parses (the existing example-parse test covers it once the keys are added, uncommented or not — keep them commented).
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — PASS; `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 5: Commit** `feat(autonomous): wire the consult room` (and `docs(autonomous): document consult_room` if split).
