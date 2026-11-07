# A consult room for autonomous sessions (`ask_human`)

- **Scope**: `crates/sapphire-agent-server/` (`channel/mod.rs`, `channel/discord.rs`, `channel/matrix.rs`,
  `background.rs`, a new `tools/ask_human.rs`, `agent.rs`'s `run`, `autonomous.rs`, `TurnContext` in
  `serve/mod.rs`, `config.rs`, `config.example.toml`, `verify`)
- **References**: `docs/superpowers/specs/2026-11-06-background-tasks-design.md` (the layer this spec builds on;
  "the layer spec" below), `src/placement.rs` (which host runs channels and background work)
- **Status**: design

## Why

An autonomous refactoring session runs into unclear points in its spec. Today it can only guess or stop.
We want it to ask a person, keep working while it waits, and continue with the answer when it arrives
(item D of the layer spec).

The questions go to a dedicated chat room. The first deployment is Discord, but we don't want to be
locked into Discord: a Matrix room must work the same way, and supporting or migrating to another service
later (Nextcloud Talk, for example) must be easy. So the only thing asked of a platform is **replies**;
threads are not assumed.

## Decisions

1. **A question is a kind of background task (`TaskKind::Question`).** It reuses the layer's inbox,
   restart handling and `WAIT` resume. Running the consult room as its own conversation session that relays
   to others was rejected: it adds an LLM, and puts two model hops between the person's answer and the session.
2. **Questions and answers are matched by reply.** No question ID (decision 6 of the layer spec). The IDs of
   the posted messages are recorded, and a reply to any of them is the answer.
3. **Only autonomous sessions can ask.** Background subagents started by an autonomous session can ask too,
   but the question is registered to the parent (the autonomous session), and the answer goes there.
   ACP and chat turns don't get the tool.
4. **With no reply, the session gets "no answer" at the deadline** (24 hours by default). The session decides
   for itself whether to proceed on a guess or stop.
5. **Replies after the answer are delivered as follow-ups.** Answered questions are kept until the session ends.
6. **Messages in the consult room never reach normal chat.** A message that isn't a reply gets a short note.
7. **Platform-specific code lives only in the `Channel` implementations.** Matching, state and deadlines are
   shared agent code. Discord and Matrix are both implemented from the start.

## Structure

### Channels (`channel/`)

A `Channel` implementation provides exactly three things:

| Capability | Trait | Discord | Matrix |
|---|---|---|---|
| Return the IDs of posted messages | `send -> Result<Vec<String>>` | message IDs (several when split) | the event ID from `room.send`'s response |
| Carry the reply target on incoming messages | `IncomingMessage.reply_to: Option<String>` | `message_reference.message_id` | `Relation::Reply`'s `in_reply_to`; for a reply inside a thread, `Relation::Thread`'s `in_reply_to` when it is not `is_falling_back` |
| Post as a reply | `OutgoingMessage.reply_to: Option<String>` | a message reference | an `in_reply_to` relation |

- Replies are sent through `send`; there is no separate method. When a message is split, only the first
  part is a reply.
- Existing callers that don't need the IDs ignore them. `Channels` (the dispatcher) also returns `Vec<String>`.
- **Matrix reply fallback**: some clients prefix a reply's body with a quote of the original
  (`> <@user> …` lines and a blank line). When `reply_to` is set, the leading run of `>` lines and one blank
  line after it are removed (a pure function, `strip_reply_fallback`).
- A channel without reply support always leaves `reply_to` as `None` (the trait default). Support is queried
  with `Channel::supports_replies(&self) -> bool` (default `false`; `true` for Discord and Matrix).

### The layer (`background.rs`)

```rust
pub(crate) enum TaskKind {
    Subagent { agent: String, handle: String },
    Question {
        room: String,
        message_ids: Vec<String>,
        question: String,
        /// The subagent handle when a child asked; None when the autonomous session itself asked.
        asked_by: Option<String>,
        deadline: DateTime<Utc>,
    },
}

pub(crate) enum Outcome {
    // in addition to the existing Finished / Failed / Interrupted / Cancelled
    Answered { text: String, from: String },
    NoAnswer,
    FollowUp { text: String, from: String },
}

pub(crate) enum Route {
    /// A reply to a question still waiting for its answer.
    Pending { session: String, id: TaskId },
    /// A reply to a question already answered, or past its deadline.
    Answered { session: String, id: TaskId },
    /// A withdrawn question, a question of a finished session, or an unknown message.
    Unknown,
}
```

- `SessionTasks` gains `closed_questions: Vec<PendingTask>` (answered and expired questions), persisted.
  `cancel_all` (session teardown) forgets them with the rest.
- `open` (load at start): a pending `Question` stays pending instead of becoming interrupted. A `Subagent`
  is interrupted as before.
- `route_reply(room, reply_to) -> Route`: searches every session's pending tasks and `closed_questions`
  by `room` and `message_ids`.
- `answer(session, id, text, from) -> bool`: completes a pending question as `Answered` and moves it to
  `closed_questions`.
- `follow_up(session, id, text, from)`: queues a `FollowUp` completion in the inbox (no new pending task).
- `expire(session, id) -> bool`: if still pending, completes it as `NoAnswer` and moves it to
  `closed_questions`.
- `open_questions(session) -> usize`. The limit is 3 per session (a constant). Questions don't count
  toward `max_per_session` (the subagent limit).
- `cancel` (`background_task cancel`) works on questions too (withdraws them). `cancel_all` returns the
  withdrawn pending questions so the caller can post a withdrawal note. Only the return value is added; it
  is still teardown.
- `render` shows a `Question` like this, always including the question's first line:

```
[Question bg-1a2b3c4d answered by <from>: "<first line of the question>"]
<answer>

[Question bg-1a2b3c4d: no answer by the deadline: "<first line of the question>"]
Decide for yourself whether to proceed on a guess or stop; if you proceed, say so in your report.

[Question bg-1a2b3c4d follow-up by <from>: "<first line of the question>"]
<text>
```

  With `asked_by`, it ends with `To pass this to the subagent, resume handle <h>.`
- `has_pending_or_inbox` counts pending questions, so an autonomous session that replied `WAIT` while a
  question is open resumes on the answer, on the deadline, or on a follow-up.

### The tool (`tools/ask_human.rs`)

`ask_human { question: string }`, kind `Think`. It does post outside, but the destination is fixed by
config and the model can't choose it. Being `Think`, it runs without approval even under the default
autonomous origin `channel` (the policy table is unchanged).

- **When it is offered**: `[autonomous] consult_room` is set, and the turn belongs to an autonomous session.
  `TurnHost` gains `consult_identity() -> Option<ConsultIdentity>` (task name, owning session, and the
  asking subagent's handle), returned by the autonomous host. `detached()` keeps it, and a background
  child's host forwards the parent's with its own handle as `asked_by`.
- **Behaviour**:
  1. With 3 open questions, refuse: `You already have 3 questions waiting for an answer. Wait for one, or withdraw one with background_task.`
  2. Refuse when the consult room isn't served on this host (`placement`), or its channel doesn't support
     replies.
  3. Post:
     ```
     [Autonomous: <task name>] question from subagent <handle>   <- the second part only when a child asked
     <question>
     (Reply to this message to answer. Waiting until <deadline, local time>.)
     ```
  4. If posting fails, register nothing and return the error.
  5. On success, register a `TaskKind::Question`, start its deadline timer, and return:
     `Question {id} posted. The answer will be delivered to this conversation when it arrives; keep working or reply WAIT.`
     When a child asked: `Question {id} posted. The answer goes to the session that started you, not to you; finish what you can and mention the question in your report.`

### Receiving (`agent.rs` `run`)

Messages in the consult room are taken aside before `handle_message` and passed to `route_reply`.

| Route | Action |
|---|---|
| `Pending` | `answer`. The bot stays silent, so it reads as a natural conversation |
| `Answered` | `follow_up` |
| `Unknown` (a reply to something unknown) | the bot replies `That question has ended (withdrawn, or its session finished).` |
| not a reply | the bot replies `Reply to a question to answer it.` |

- Only the text is delivered. If an image is attached, `(an image was attached but not forwarded)` is
  appended; a reply with only an image delivers just that note.
- `from` is `IncomingMessage.sender` as is. The listener still filters by `allowed_users`.
- Consult-room messages create no session, so they don't count in the autonomous idle check (`is_idle`).

### Deadlines

- At registration, a `tokio::time::sleep_until(deadline)` task is started. When it fires and `expire`
  returns true, the bot replies to the question with
  `No answer by the deadline; the session will decide on its own. A reply now still reaches it as extra information.`
- At start, deadlines of the questions `open` kept are re-armed (expired at once if already past).
- Withdrawal notes: for each question `cancel` / `cancel_all` returns, the bot replies
  `This question was withdrawn.`

### Config

```toml
[autonomous]
consult_room = "123456789012345678"   # or "!room:example.org"
consult_timeout_hours = 24
```

- Without `consult_room`, `ask_human` isn't offered.
- `verify` warns when the consult room isn't among any channel's rooms (Discord `channel_ids` / Matrix
  `room_ids`), when `[autonomous]` is disabled, and when the room's channel doesn't support replies.
- Synced hosts: autonomous sessions run on the primary device. If the channel serving the consult room is on
  another host, `ask_human` is refused (`consult room <room> is not served on this host`), and a warning is
  logged at start.

## Failures and edge cases

- Posting fails → the tool returns an error; nothing is registered.
- A bot note (deadline, withdrawal, routing) fails to send → a warning in the log only; no state changes.
- Saving fails → as in the layer (a warning; the in-memory state wins).
- The session closes (DONE, or out of turns) → its open questions are withdrawn with a note, and its answered
  questions are forgotten. Later replies route to `Unknown`.
- Replies sent while the agent is restarting are missed (Discord / Matrix history is not replayed). The
  deadline or a later reply covers it.

## Tests

Run the flow end to end with a fake `Channel` that records posts, hands out IDs, and lets the test inject
incoming messages.

- ask → answer → a session that replied `WAIT` resumes and receives the answer
- ask → deadline → `NoAnswer` is delivered and the deadline reply appears in the room
- a reply after the answer is delivered as a `FollowUp`
- a reply to a withdrawn question gets the `Unknown` note
- a message that isn't a reply gets the note and never reaches normal chat
- after a restart (`open`), a question stays pending and its deadline is re-armed
- a 4th question is refused
- a child's question is registered to the parent session, with the `resume handle` hint
- consult-room messages don't count in the idle check
- pure functions: extracting the reply target for Discord / Matrix, `strip_reply_fallback`

## Out of scope

- Channels other than Matrix and Discord (Nextcloud Talk and others): implementing the three capabilities
  is enough to add one.
- More than one consult room, or choosing a destination per question.
- Picking up replies sent during a restart from history.
- Questions from ACP or chat turns.

## Commit scopes

`feat(tools)` / `feat(autonomous)` / `feat(channel)` / `feat(matrix)` / `feat(discord)`. All are
agent-internal scopes, so `cliff.toml` doesn't change.
