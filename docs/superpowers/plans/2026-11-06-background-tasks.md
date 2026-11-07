# バックグラウンドタスク層と、サブエージェントのバックグラウンド実行 — 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `subagent` を `background: true` で走らせ、完了を親セッションの受け箱経由で次のターン／ラウンドの合間に届ける。自律セッションは `WAIT` で休止し、完了が届いたら同じセッションを再開する。

**Architecture:** 新設 `src/background.rs` の `BackgroundTasks` を `ServeState` が 1 つ持ち、セッションごとの保留と受け箱をメモリで管理し、ワークスペース外の JSON に写す。配達は `run_llm_turn`（ターン開始）と `TurnLoop::run`（ラウンドの合間）で行う。バックグラウンドの子は `BackgroundSubagentHost` の下で走り、`Ask` を保存済みの答えだけで決める。

**Tech Stack:** Rust、tokio、serde_json、既存の `TurnHost` / `TurnLoop` / `SubagentCache` / `PermissionStore`。

**Spec:** `docs/superpowers/specs/2026-11-06-background-tasks-design.md`

## Global Constraints

- パスはすべて `crates/sapphire-agent-server/src/` からの相対。テストは `cargo test -p sapphire-agent-server`。
- 設定は `[tools.background] max_per_session`、既定 `4`。`0` でバックグラウンド実行を無効化（`background` をスキーマに出さず、`background_task` を登録しない）。
- タスク id は `bg-` + uuid v7 の先頭 8 桁（16 進）。
- 保存先は `~/.cache/sapphire-agent/background/<session_id>.json`（`dirs::cache_dir()`、`SubagentCache::default_dir` の隣）。一時ファイル + rename。読めないファイルは空扱い。
- バックグラウンドは `TurnContext::session_id.is_some() && subagent_depth == 0` のときだけ。
- 完了テキストの見出しは `[Background task {id} {verb}: subagent "{agent}" (handle {handle})]`。`verb` は `finished` / `failed` / `was interrupted by a restart` / `was cancelled`。拒否行は `Refused (needs approval): {name} {input_json}`。
- 自律の返事の約束ごと: `DONE`（終了、`cancel_all`）、`WAIT`（保留を待つ。保留も受け箱も空ならふつうの続行）。
- コミットの scope は `feat(tools)` / `feat(autonomous)`。`cliff.toml` は変えない。各コミットの末尾に `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。
- 表示するパスは `workspace::slash_path`。POSIX シェルを使うテストは `cfg(unix)`。

## Review Focus

1. **同じラウンドで二つの完了が届く** — 両方とも `finished_at` 順に一つのメッセージへ入る。→ Task 3 のテスト `two_completions_arrive_in_one_round_in_finish_order`。
2. **取り消しと完了が競合する** — `cancel` の直後に子が `complete` しても、完了は一度だけ（`Cancelled`）。→ Task 1 のテスト `complete_after_cancel_is_a_no_op`。
3. **バックグラウンドで実行中のハンドルを `resume` する** — 「使用中」で断られ、子は止まらない。→ Task 5 のテスト `resuming_a_handle_running_in_background_is_refused`。
4. **自律タスクの休止中にサーバーが再起動する** — 保留が `Interrupted` になり、次のサイクルで同じセッションが再開する。→ Task 7 のテスト `a_suspended_task_resumes_after_a_restart`。
5. **自律の既定 `origin = "channel"` では `subagent` が拒否される**（kind `Other`）— 今の挙動のまま。設定例のコメントに「自律でサブエージェントを使うには `trusted`」と書く。→ Task 8。

---

### Task 1: `BackgroundTasks` の中核（メモリ上）と設定

**Files:**
- Create: `background.rs`
- Modify: `lib.rs`（`mod background;`）、`config.rs`（`ToolsConfig` に `background: BackgroundConfig`）

**Interfaces:**
- Produces:
  ```rust
  pub(crate) type TaskId = String;
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub(crate) enum TaskKind { Subagent { agent: String, handle: String } }
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub(crate) struct PendingTask { pub id: TaskId, pub kind: TaskKind, pub label: String, pub started_at: DateTime<Utc> }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub(crate) struct RefusedCall { pub name: String, pub input: serde_json::Value }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub(crate) enum Outcome { Finished { text: String, refused: Vec<RefusedCall> }, Failed { message: String }, Interrupted, Cancelled }
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub(crate) struct Completion { pub task_id: TaskId, pub kind: TaskKind, pub outcome: Outcome, pub finished_at: DateTime<Utc> }

  pub(crate) struct BackgroundTasks { /* dir: Option<PathBuf>, max_per_session, std::sync::Mutex<HashMap<String, SessionTasks>> */ }
  impl BackgroundTasks {
      pub(crate) fn in_memory(max_per_session: usize) -> Arc<Self>;
      pub(crate) fn enabled(&self) -> bool;                 // max_per_session > 0
      pub(crate) fn register(&self, session: &str, kind: TaskKind, label: String) -> anyhow::Result<TaskId>;
      pub(crate) fn attach(&self, session: &str, id: &str, abort: tokio::task::AbortHandle);
      pub(crate) fn complete(&self, session: &str, id: &str, outcome: Outcome) -> bool; // false if id not pending
      pub(crate) fn drain(&self, session: &str) -> Vec<Completion>;   // marks in-flight, sorted by finished_at
      pub(crate) fn commit_delivery(&self, session: &str);            // drops in-flight entries
      pub(crate) fn pending(&self, session: &str) -> Vec<PendingTask>;
      pub(crate) fn has_inbox(&self, session: &str) -> bool;          // undelivered, not in-flight
      pub(crate) fn has_pending_or_inbox(&self, session: &str) -> bool;
      pub(crate) fn cancel(&self, session: &str, id: &str) -> anyhow::Result<()>;
      pub(crate) fn cancel_all(&self, session: &str);
  }
  pub(crate) fn render(c: &Completion) -> String;
  // config.rs
  pub struct BackgroundConfig { pub max_per_session: usize }  // serde default 4
  ```

- [ ] **Step 1: Write the failing tests** in `background.rs` `mod tests`:
  - `register_refuses_past_the_cap_naming_every_held_id`: `in_memory(2)`、2 件登録後の 3 件目が `Err`、メッセージに両 id と `background_task` を含む。
  - `register_with_zero_cap_is_refused`: `in_memory(0)` で `Err`、`enabled()` が `false`。
  - `complete_moves_pending_to_inbox`: 登録 → `complete(Finished{..})` が `true`、`pending` 空、`has_inbox` 真、`drain` が 1 件。
  - `drain_then_commit_empties_the_inbox`: `drain` 後は `has_inbox` 偽だが `has_pending_or_inbox` は `commit_delivery` までは真、後は偽。
  - `complete_after_cancel_is_a_no_op`: `cancel` 後の `complete` が `false`、`drain` は `Cancelled` 1 件だけ。
  - `cancel_unknown_id_lists_running_ids`: `Err` のメッセージに実行中の id。
  - `cancel_all_aborts_attached_tasks`: `tokio::spawn(pending())` の `AbortHandle` を `attach`、`cancel_all` 後に `JoinHandle` が `is_cancelled()`。
  - `render_formats_each_outcome`: `Finished` に `refused` 1 件 → 見出し `[Background task bg-1 finished: subagent "reviewer" (handle h1)]`、本文、`Refused (needs approval): shell {"command":"cargo test"}`。`Interrupted` → `was interrupted by a restart` と `pass handle h1 as \`resume\``。
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server background::` — FAIL（モジュールがない）。
- [ ] **Step 3: Implement** 上の型と API。`register` は上限確認と挿入を 1 回のロックで行う。id は `format!("bg-{}", &Uuid::now_v7().simple().to_string()[..8])`。`render` の `Failed` は本文に `message`、`Interrupted` / `Cancelled` は resume の案内一行。`BackgroundConfig` を `ToolsConfig` に `#[serde(default)]` で足す。
- [ ] **Step 4: Run** 同コマンド — PASS。`cargo test -p sapphire-agent-server config` も PASS（`config.example.toml` を読むテストがある）。
- [ ] **Step 5: Commit** `feat(tools): background task registry`

### Task 2: 保存と起動時の回復、`ServeState` への配線

**Files:**
- Modify: `background.rs`、`subagent_cache.rs`（ファイル名の検査を `pub(crate) fn check_file_stem(s: &str) -> anyhow::Result<()>` に切り出し、`path_for` から使う）、`serve/mod.rs`（`ServeState` に `pub(crate) background: Arc<BackgroundTasks>`、`build_for_test*` は `BackgroundTasks::in_memory(4)`）、`lib.rs`（`BackgroundTasks::open(BackgroundTasks::default_dir(), config.tools.background.max_per_session)`）

**Interfaces:**
- Consumes: Task 1。
- Produces: `pub(crate) fn open(dir: Option<PathBuf>, max_per_session: usize) -> Arc<Self>`、`pub(crate) fn default_dir() -> Option<PathBuf>`、`ServeState::background`。

- [ ] **Step 1: Write the failing tests:**
  - `state_round_trips_through_the_file`: tempdir で `open` → 登録・完了 → 新しい `open` で同じセッションの `has_inbox` が真。
  - `pending_left_on_disk_becomes_interrupted_on_open`: 登録だけして `open` し直す → `pending` 空、`drain` が `Outcome::Interrupted`、`kind` の handle が残る。
  - `in_flight_entries_survive_a_crash_before_commit`: `drain` 後に `commit_delivery` せず `open` し直す → 同じ完了がもう一度 `drain` される。
  - `an_unreadable_file_is_empty_not_fatal`: 壊れた JSON を置いて `open` → パニックせず空。
  - `an_unsafe_session_id_is_not_written`: `"../x"` で登録しても dir の外にファイルができない（メモリ上は動く）。
  - `cancel_all_removes_the_file`。
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server background::` — FAIL。
- [ ] **Step 3: Implement** 状態が変わるたびにそのセッションの `{pending, inbox}`（in-flight 含む）を書く。書けなければ `warn!`。`open` は dir 内の `*.json` を読み、pending を `Interrupted` の完了に変えて書き直す。古い `.tmp` は無視。
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS（`ServeState` の全構築箇所がコンパイルする）。
- [ ] **Step 5: Commit** `feat(tools): persist background tasks and recover them on start`

### Task 3: 完了の配達（ターン開始とラウンドの合間）

**Files:**
- Modify: `serve/mod.rs`（`run_llm_turn` で `user_msg` を履歴へ入れる直前、`TurnLoop::run` で `result_msg` を作った直後）

**Interfaces:**
- Consumes: `ServeState::background`、`drain`、`commit_delivery`、`render`。
- Produces: `fn deliver_into(state: &ServeState, session_id: &str, msg: &mut ChatMessage) -> bool`（`serve/mod.rs` 内、private）。完了ごとに `ContentPart::Text(render(c))` を足す。ターン開始は `parts` の先頭へ挿入、ラウンドの合間は末尾へ追加。

- [ ] **Step 1: Write the failing tests** (`serve/mod.rs` の tests、`ServeState::for_test_scripted`):
  - `a_completion_is_prepended_to_the_next_turn`: 事前に `state.background` へ登録＋完了 → `run_llm_turn` → 保存された最初のユーザーメッセージの最初の part が見出しを含み、元の本文が続く。`has_pending_or_inbox` が偽。
  - `a_completion_lands_between_rounds_beside_tool_results`: スクリプト 1 ラウンド目でツール呼び出し、テスト用ツールの `execute` の中で `complete` する → 2 回目のプロバイダ呼び出しに渡ったツール結果メッセージが `ToolResult` の後に見出しの `Text` を持つ。tool_use と tool_result の対が保たれる（`repair_tool_pairing` で何も落ちない）。
  - `two_completions_arrive_in_one_round_in_finish_order`。
  - `a_subagents_nested_loop_delivers_nothing`: `persistence: None` の `TurnLoop` では `drain` されない。
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server serve::` — FAIL。
- [ ] **Step 3: Implement** `deliver_into` を二箇所で呼び、それぞれ永続化（`append` / `append_message_paired`）の後に `commit_delivery`。
- [ ] **Step 4: Run** — PASS。
- [ ] **Step 5: Commit** `feat(tools): deliver background completions at turn start and between rounds`

### Task 4: `TurnHost` の追加と `BackgroundSubagentHost`

**Files:**
- Modify: `serve/mod.rs`（`TurnHost`、`TurnLoop::run` の `Decision::Ask` 分岐）、`serve/acp.rs`（`AcpProgress`）、`tools/policy.rs`（`Refusal::NeedsApproval`）、`tools/subagent.rs`（`BackgroundSubagentHost`）

**Interfaces:**
- Produces:
  ```rust
  // TurnHost（既定実装つき）
  async fn standing_approval(&self, call: &ToolCall, kind: ToolKind) -> Option<Approval> { None }
  fn declined_refusal(&self) -> Refusal { Refusal::UserDeclined }
  async fn background_finished(&self, text: &str) {}
  // policy.rs
  Refusal::NeedsApproval  // "{name} needs the user's approval, which a background task cannot ask for. Report it in your answer instead of retrying."
  // subagent.rs
  pub(crate) struct BackgroundSubagentHost { parent: Arc<dyn TurnHost>, refused: Arc<std::sync::Mutex<Vec<RefusedCall>>> }
  ```
  `TurnLoop::run` の `Ask` 分岐は、拒否のとき `refusal_message(&call.name, progress.declined_refusal())` を使う。
  `AcpProgress::standing_approval` は `self.permissions.standing(&self.profile, &call.name)` を `AllowAlways` / `RejectAlways` に写す。`AcpProgress::approve` の冒頭はこれを使うよう整理する。
  `AcpProgress::background_finished` は `SessionUpdate::AgentThoughtChunk` で `text` を送る（失敗はログのみ）。
  `BackgroundSubagentHost`: `origin` / `acp_client` / `cwd` / `client_fs_caps` / `client_terminal_cap` は親へ転送。`tool_start` / `tool_end` は `debug!` のみ。`turn_error` / `message_chunk` は握りつぶす。`round_budget` は `Unattended`。`avatar` は `None`。`approve` は `parent.standing_approval` が許可ならそれを返し、そうでなければ `RefusedCall` を記録して `RejectOnce`。`declined_refusal` は `NeedsApproval`。

- [ ] **Step 1: Write the failing tests** (`tools/subagent.rs` tests):
  - `background_host_refuses_an_ask_and_records_it`: `standing_approval` が `None` の親 → `approve` が `RejectOnce`、`refused` に `{name, input}`。
  - `background_host_runs_a_standing_allow`: 親が `Some(AllowAlways)` → `allows()`、記録なし。
  - `background_host_forwards_origin_but_not_tool_events`: 親の `origin` が返り、親の `tool_start` は呼ばれない。
  - `serve::` 側: `an_ask_declined_by_a_background_host_reads_needs_approval`: `TurnLoop` を `Acp(Default)` + `Other` 種のツールで回し、ツール結果が `NeedsApproval` の文言。
- [ ] **Step 2: Run** — FAIL。
- [ ] **Step 3: Implement** 上記。
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS（既存の ACP 許可テストも含む）。
- [ ] **Step 5: Commit** `feat(tools): a host for background subagents that never asks`

### Task 5: `subagent` の `background: true`

**Files:**
- Modify: `tools/subagent.rs`、`lib.rs:612`（`SubagentTool::new(agent_defs, config.tools.background.max_per_session > 0)`）、`tools/mod.rs:584` と `tools/config_tools.rs:1654`（`new(.., false)`）

**Interfaces:**
- Consumes: Task 1・4。
- Produces:
  - `SubagentTool::new(agents: Vec<AgentDef>, background: bool) -> Self`、`build_spec(agents: &[AgentDef], background: bool) -> ToolSpec`（`subagent_tool_specs` が作る入れ子用の spec は常に `false`）。スキーマに `"background": {"type":"boolean","description":"Run it in the background and return at once; the result is delivered to this conversation when it finishes. Main conversation only."}`。
  - `busy_handles: Arc<std::sync::Mutex<HashSet<String>>>`、`ResumeGuard` は `Arc` を持つ所有型にして spawn したタスクへ move できるようにする。新規の dispatch でも、background のときはハンドルを claim する。
  - `impl Tool for Arc<SubagentTool>` の `execute` は `self.execute_arc(input)` を呼ぶ: `async fn execute_arc(self: &Arc<Self>, input: &Value) -> anyhow::Result<String>`。
  - `run_and_store` に `progress: Arc<dyn TurnHost>` と `namespace: String` を引数で渡す（同期経路は今の `SubagentHost` と `current_memory_namespace()` を渡す）。
  - 戻り文: `Started background task {id}: subagent "{agent}" (handle {handle}). Its result will be delivered to this conversation when it finishes.`

- [ ] **Step 1: Write the failing tests** (`turn_context_with_host` に `session_id: Some(..)` を渡す版を足す。子と親の応答が競合しないよう、`system` に子の定義名を含むかで台本を切り替える `ByPromptProvider` をテスト内に作る):
  - `background_dispatch_returns_at_once_and_completes_later`: 子のプロバイダが `Notify` で止まっている間に `execute` が戻り文を返す。解放後 `state.background.drain` が `Finished`、`text` が子の答え、`subagent_cache` に handle が保存されている。
  - `background_completion_carries_refused_calls`: 子が `Other` 種のツールを呼び、`Acp(Default)` の親 → 完了の `refused` に 1 件。
  - `background_is_refused_without_a_session` / `background_is_refused_at_depth_one`: メッセージに `main conversation`。
  - `background_is_refused_past_the_cap`。
  - `resuming_a_handle_running_in_background_is_refused`: 子が止まっている間に同じ handle で `resume` → `already in use`。
  - `a_background_timeout_completes_as_failed_with_the_handle`: `turn_timeout_secs = 1` で止まる子 → `Failed`、`message` に handle。
  - `the_nested_spec_never_offers_background`: `subagent_tool_specs(..)` の `subagent` スキーマに `background` がない。`new(.., false)` の spec にもない。
  - `background_survives_the_parent_turn_ending`: `run_llm_turn` が返った後も子が走り、完了する。
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server subagent::` — FAIL。
- [ ] **Step 3: Implement** spec の「流れ」1〜6。spawn する future は `scope_turn_context`（同じ `ctx`）、`scope_memory_namespace`、`scope_admin_room_profile`、`timer_origin`、`acp_client` を明示的にかけ直す（`TurnLoop::run` のツール実行と同じ並び）。`register` の直後に `tokio::spawn`、`attach(abort_handle())`。終了時に `complete`、続けて `ctx.progress.background_finished(&render(..))`。
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS。
- [ ] **Step 5: Commit** `feat(tools): run subagents in the background`

### Task 6: `background_task` ツール

**Files:**
- Create: `tools/background_tools.rs`
- Modify: `tools/mod.rs`（`mod background_tools;`）、`lib.rs`（`max_per_session > 0` のとき登録）

**Interfaces:**
- Consumes: `current_turn_context()`、`BackgroundTasks::{pending, cancel}`。
- Produces: `pub struct BackgroundTaskTool`、名前 `background_task`、`kind() = ToolKind::Think`。入力 `{"action": "list" | "cancel", "id"?: string}`。セッションのないターンでは `Err`。

- [ ] **Step 1: Write the failing tests:**
  - `list_shows_id_label_and_elapsed`: 1 件登録 → 出力に id とラベルと `s ago`（秒）。空なら `No background tasks are running.`。
  - `cancel_stops_a_task_and_queues_cancelled`。
  - `cancel_without_id_is_an_error` / `unknown_action_is_an_error`。
  - `refused_without_a_session`。
- [ ] **Step 2: Run** — FAIL。
- [ ] **Step 3: Implement**。
- [ ] **Step 4: Run** — PASS。
- [ ] **Step 5: Commit** `feat(tools): list and cancel background tasks`

### Task 7: 自律セッションの `WAIT` と休止・再開

**Files:**
- Modify: `autonomous.rs`、`tools/config_tools.rs:806`（`task_test` の `finish` でも `state.background.cancel_all(session_id)`）

**Interfaces:**
- Consumes: `BackgroundTasks::{has_inbox, has_pending_or_inbox, cancel_all}`。
- Produces:
  - `CONTINUE_PROMPT = "Continue the task. When it is finished, reply with exactly:\nDONE\nIf you are only waiting on background tasks, reply with exactly:\nWAIT"`。
  - `fn suspended_session(task: &AutonomousTask, rows: &[SessionRow], bg: &BackgroundTasks) -> Option<String>`: `room_id == task.name && !is_closed` かつ `has_pending_or_inbox` のセッション（最新）。
  - `fn turns_spent(store: &SessionStore, session_id: &str, task: &str) -> usize`: `load_session` のユーザーメッセージのうち、テキストが `marker(task)` で始まるものの数。
  - `run_task(&mut self, task, store, resume: Option<String>)`。`resume` が `Some` なら新規作成せず、`turns = turns_spent(..)` から始め、最初の送信は `CONTINUE_PROMPT` 版。
  - `run_cycle`: 有効なタスクを優先順に見て、`suspended_session` があり `has_inbox` が真のものを最初に再開する（クールダウンと `not_before` は見ない）。なければ `next_task` で、休止中（`suspended_session` が `Some`）のタスクを除いて選ぶ。
  - 終了の分岐: `DONE` / spent / capped → `cancel_all` してから `close_session`（今の `not_before` も維持）。`WAIT` かつ `has_pending_or_inbox` → セッションを閉じず、`not_before` も設定せず、`write_state("idle", "task: {name} waiting on background tasks")` で戻る。`WAIT` で何も待っていなければ続行。

- [ ] **Step 1: Write the failing tests** (`autonomous.rs` tests、`for_test_scripted`):
  - `wait_with_a_pending_task_suspends_without_closing`: 1 ターン目の前に `state.background` へ登録しておき（ターン中に登録するツールの代わり）、応答 `WAIT` → セッションが閉じていない。
  - `a_suspended_task_is_skipped_while_its_inbox_is_empty_and_another_runs`: 休止中の A と、通常の B → 次のサイクルで B のセッションが作られる。
  - `a_completion_resumes_the_same_session`: `complete` 後のサイクルで、新しいセッションが作られず、A のセッションに見出しを含むユーザーメッセージが増える。
  - `suspension_does_not_spend_max_turns`: `max_turns: 2`、1 ターン目 `WAIT`、再開後 `working`、`DONE` → 合計 3 回目は走らず、2 ターン目で capped（`WAIT` のターンは 1 と数える）。
  - `done_cancels_pending_tasks`: 登録済みのまま `DONE` → `has_pending_or_inbox` が偽。
  - `wait_with_nothing_pending_continues`: 保留なしの `WAIT` → 閉じずに次のターンへ進み、`max_turns` で閉じる。
  - `a_suspended_task_resumes_after_a_restart`: tempdir の `BackgroundTasks::open` で登録 → `WAIT` で休止 → 同じ dir で `open` し直した `BackgroundTasks` を入れた state でサイクル → 再開し、送られたメッセージに `was interrupted by a restart`。
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server autonomous::` — FAIL。
- [ ] **Step 3: Implement** 上記。モジュール冒頭のドキュメントと「There is no resume」のコメントを、休止と再開を書いたものに改める（設計判断 13 の改訂）。
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS。
- [ ] **Step 5: Commit** `feat(autonomous): suspend on WAIT and resume when a background task completes`

### Task 8: 設定例と仕上げ

**Files:**
- Modify: `crates/sapphire-agent-server/config.example.toml`（`[tools.subagent]` の説明の後に `[tools.background]` の説明と例。`[autonomous]` の `origin` の説明に「`subagent` は `Other` 種なので、自律でサブエージェント（バックグラウンド含む）を使うには `trusted`」）、spec の「流れ 2」（ハンドルはもともと開始時に決まっていた）と `wait()` の記述を実装に合わせて直す

- [ ] **Step 1: Run** `cargo test -p sapphire-agent-server config` — `config.example.toml` を読むテストが PASS。
- [ ] **Step 2: Run** `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all --check` — 警告なし。
- [ ] **Step 3: Run** `cargo test --workspace` — PASS。
- [ ] **Step 4: Commit** `docs(tools): document [tools.background]`
