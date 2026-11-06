# バックグラウンドタスク層と、サブエージェントのバックグラウンド実行

- **対象**: `crates/sapphire-agent-server/`（新設モジュール `background`、`tools/subagent.rs`、
  `serve/mod.rs` の `TurnHost` / `TurnLoop` / `run_llm_turn`、`autonomous.rs`、`config.rs`、
  `config.example.toml`）
- **参照**: `src/tools/subagent.rs`（`SubagentHost`、`busy_handles`、`run_and_store`）、
  `src/subagent_cache.rs`（ワークスペース外の保存の流儀）、
  `src/tools/client_exec.rs` の `try_reserve_terminal_slot`（予約と作成を一度のロックで行う前例）、
  `src/serve/acp_permissions.rs`（`PermissionStore`）、`src/autonomous.rs`
- **状態**: 設計

## なぜ

今でも、1 つのアシスタントメッセージに含まれる複数のツール呼び出しは並行に動く
（チャンネル経路は `agent.rs` の `tokio::spawn`、`run_llm_turn` は `join_all`）。
足りないのは **バックグラウンド実行** だ。`subagent` は終わるまで親のターンを止めるので、
親が子を走らせたまま別の作業を続け、終わったら結果を受け取る、という使い方ができない。

想定する使い方は開発で、二つある。

- **自律セッション**でのリファクタリング。親が実装・レビューの子を走らせながら別の作業を進める。
- **ACP（エディタ）**での利用。

将来は、自律セッションが不明点を Discord の専用チャンネルに相談し、
返事を待つ間も別の作業を続ける（以下 D）。D は別の spec にするが、
この層は D の返事も受けられる形にしておく。

## 決めたこと

1. **完了の届け先は、セッションの受け箱。** 完了はセッションごとの受け箱に積み、
   そのセッションの次のターンの開始時と、実行中のターンのラウンドの合間に差し込む。
   履歴へ直接追記はしない（実行中のターンと競合し、tool_use / tool_result の対を崩すため）。
2. **保留と受け箱はワークスペースの外に保存する。** 再起動で黙って消えないように、
   起動時に残っていた保留は「中断された」完了に変えて届ける。
   実行中のタスクそのものは再起動で止まる。
3. **バックグラウンドは、セッションのあるメインターン（深さ 0）からだけ。**
   ネストしたターンには `session_id` がなく、結果を届ける会話も残らない。
4. **バックグラウンドの子は、許可を尋ねない。** 許可の表（`Origin`）は親のものをそのまま使うが、
   `Ask` になった呼び出しは保存済みの「常に許可 / 常に拒否」だけで決め、それ以外は拒否する。
   答えのなかった `Ask` だけを完了に載せて親に返す（「常に拒否」は載せない）。親は自分のターンで（人間の許可を得て）実行するか、
   `resume` で子に続けさせる。**親のモデルに許可を出させることはしない**：
   子に頼んだ当のモデルが承認すると、人間による確認が実質なくなるうえ、
   子が読んだ内容に仕込まれた指示に親が説得されうる。
5. **自律セッションは `WAIT` で休止し、完了が届いたら同じセッションを再開する。**
   待つ間も他の自律タスクは動く。これは autonomous の設計判断 13（休止状態を持たない）を改める。
6. **D の返事は、質問 ID ではなく返信で対応付ける**（D の spec で詳述）。
   この層への要件は、1 セッションが保留を複数持て、それぞれが独立に完了することだけ。

## 構成

### `src/background.rs` — `BackgroundTasks`

`ServeState` が 1 つ持つ。単位はセッション（`session_id`）。

```rust
pub(crate) struct PendingTask {
    pub id: TaskId,              // "bg-<短い uuid>"
    pub kind: TaskKind,
    pub label: String,           // モデルとログに見せる一行の説明
    pub started_at: DateTime<Utc>,
}

pub(crate) enum TaskKind {
    Subagent { agent: String, handle: String },
    // D で Question { … } を足す
}

pub(crate) struct Completion {
    pub task_id: TaskId,
    pub kind: TaskKind,
    pub outcome: Outcome,
    pub finished_at: DateTime<Utc>,
}

pub(crate) enum Outcome {
    Finished { text: String, refused: Vec<RefusedCall> },
    Failed { message: String },
    Interrupted,                 // 再起動で止まった
    Cancelled,
}

pub(crate) struct RefusedCall {
    pub name: String,
    pub input: serde_json::Value,
}
```

セッションごとに、実行中のタスク（`PendingTask` と `abort` 用の `AbortHandle`。ハンドルは保存しない）、
`inbox: Vec<Completion>`、`drain` 済みで未確定の `in_flight` を持つ。

| API | 働き |
|---|---|
| `register(session, kind, label) -> Result<TaskId>` | 上限（`max_per_session`）の確認と登録を一度のロックで行う。超えたら、実行中の id をすべて挙げて拒否 |
| `attach(session, id, AbortHandle)` | 取り消し用に tokio タスクを結びつける |
| `complete(session, id, outcome)` | pending から外して inbox に積む |
| `drain(session) -> Vec<Completion>` | 受け箱を取り出す |
| `has_pending_or_inbox(session) -> bool` | 自律ループの休止判定用 |
| `has_inbox(session) -> bool` | 受け箱に未配達の完了があるか。自律ループが毎周期ポーリングする（`wait` は持たない） |
| `rollback_delivery(session)` | 運んだメッセージを保存できなかったとき、配達中のエントリを受け箱へ戻す |
| `cancel(session, id)` | `abort` して `Cancelled` を積む |
| `cancel_all(session)` | 後始末: `abort` し、セッションを忘れ、ファイルを消す。何も積まない |

### 保存

`SubagentCache` の隣（`<cache>/background/<session_id>.json`）に `{pending, inbox}` を書く。
ファイル名の検査、一時ファイルへ書いてから rename、「読めないものは無いものとして扱う」は
`subagent_cache.rs` の流儀に従う。

- 状態が変わるたびに書く（件数はたかが知れている）。書けなければ `warn` を出して続ける。
  メモリ上の状態が正しい。
- **起動時**: 残っている pending をすべて `Outcome::Interrupted` に変えて inbox へ移す。
  サブエージェントなら `handle` も残る。新規のディスパッチは始める前にプロンプトだけの履歴を
  `SubagentCache` に保存するので、キャッシュが設定されていれば親はそのまま `resume` できる。
- 閉じたセッションのファイルは `cancel_all` のときに消す。

### 完了テキストの形

ターン開始時もラウンドの合間も同じ。

```
[Background task bg-1a2b finished: subagent "reviewer" (handle h-…)]
<最終回答>
Refused (needs approval): shell {"command":"cargo test"}
```

`Failed` / `Interrupted` / `Cancelled` も同じ見出しで、`finished` の代わりにそれぞれ
`failed` / `was interrupted by a restart` / `was cancelled` と書き、
resume できる場合（配達の時点で `SubagentCache` にその handle の履歴がある場合）だけ handle を示す。

## サブエージェントのバックグラウンド実行

### 入力

`subagent` のスキーマに `background: boolean`（省略時 `false`）を足す。
`agent`（新規）と `resume`（継続）のどちらとも組み合わせられる。
`max_per_session = 0` のときは `background` をスキーマに出さない。

### 流れ

1. 今までどおり確認する（定義の有無、深さの上限、`resume` なら `busy_handles` の取得）。
   加えて `TurnContext::session_id` が `Some` で、`subagent_depth == 0` であることを確認する。
2. ハンドルはもともとディスパッチの開始時に決まっているので、変更は要らない。
   バックグラウンドの場合は、子が終わるまで `busy_handles` でそのハンドルも押さえる。
3. `BackgroundTasks::register` に `Subagent { agent, handle }` を登録する。
4. `tokio::spawn` で子のターンを始め、`attach` する。task-local は spawn で引き継がれないので、
   メモリの名前空間・timer origin・ACP クライアント・admin room profile・`TurnContext` を
   明示的にかけ直す（`agent.rs` の同期実行と同じ注意点）。
5. ツールの結果としてすぐに返す:
   `Started background task bg-1a2b: subagent "reviewer" (handle h-…). Its result will be delivered to this conversation when it finishes.`
6. 子が終わったら `run_and_store` で履歴を保存し、`complete(Finished { text, refused })` を呼ぶ。
   失敗や panic なら `Failed`。`[tools.subagent].turn_timeout_secs` を超えたら
   `Failed`（時間切れ、handle で resume 可）。`busy_handles` は終わった時点で外す。

### `BackgroundSubagentHost`

`SubagentHost` との違い:

- `origin()`: 親のものをそのまま返す。許可の表は同期実行と同じ。
- `approve()`（`Ask` のときだけ呼ばれる）: ダイアログを出さない。
  `TurnHost` に新しく `standing_approval(call, kind) -> Option<Approval>`（既定 `None`）を足し、
  ACP の host は `PermissionStore` の「常に許可 / 常に拒否」を返す。
  「常に許可」なら実行し、「常に拒否」ならそのまま拒否する。
  どちらもなければ `RejectOnce` にして `RefusedCall` を記録する。
- 「常に拒否」は記録せず、子には UserDeclined として見える。
  `NeedsApproval` で拒否して完了に載せるのは、答えのなかった `Ask` だけ。
  どちらにするかは `TurnHost::declined_refusal(approval)` が選ぶ。
- 子は `TurnHost::detached()` の host で動くので、始めたリクエスト（HTTP SSE）を開いたままにしない。
  既定の detached host は origin だけを持つ。ACP は自分の host の複製を持つ。
  その複製はセッションのモードを `origin()` のたびに読み直すので、子の実行中に `session/set_mode` で
  厳しいモードへ戻せば子にも効く（ターン自身の host は開始時のモードを保つ）。
- `tool_start` / `tool_end`: 転送せず、ログに出すだけ。同期の子が転送するのは
  許可ダイアログの出どころを示すためで、ここではダイアログを出さない。
  ターンの外でツール呼び出しの更新を送るとエディタを混乱させうる。
- `acp_client()`: 親のものを返す。エディタの接続が続いている間はエディタ側のツールを使え、
  切断後はそのツールがエラーになる。子はそれを読んで判断する。
- `turn_error` / `message_chunk`: `SubagentHost` と同じく握りつぶす。
- `round_budget()`: `Unattended`。

### 寿命

- 親のターンの取り消し（ACP の `session/cancel`）では止めない。
- セッションを閉じる・削除するときに `cancel_all` で止める。
- 自律セッションが `DONE` で終わるときも `cancel_all`。

### `background_task` ツール

`max_per_session > 0` のときだけ登録する。

- `action: "list"`: 実行中の id、種類、ラベル、経過時間。
- `action: "cancel", id`: 取り消す。存在しない id なら実行中の id 一覧を返す。

## 完了の届け方

`run_llm_turn` / `TurnLoop::run` の中で、経路を問わず行う（ACP、IPC チャット、`/rpc`、
自律セッション）。`persistence` のない（= セッションのない）ループでは何もしない。

- **ターン開始時**: `drain` した完了を、そのターンのユーザーメッセージの先頭に
  テキストブロックとして入れる。履歴にもそのまま保存される。
- **ラウンドの合間**: ツール結果をまとめたユーザーメッセージを作るときに `drain` し、
  空でなければ同じメッセージにテキストブロックとして足す。
- 順序は `drain` → 履歴に保存 → 受け箱のファイルを書き直す。途中で落ちると
  二度届くことはあるが、届かないことはない。メッセージを保存できなかったときは
  `rollback_delivery` で受け箱へ戻すので、失われずに再配達される。
- **ACP**: 完了の時点で `session/update` を送り、エディタに完了を知らせる。
  ターンの外では表示されないこともあるが害はない。

## 自律セッション

返事の約束ごとに `WAIT` を足す（`CONTINUE_PROMPT` にも書く）。

- `DONE`: 終わり。`cancel_all` してセッションを閉じる。
- `WAIT`: 保留中のタスクを待つ。保留も未配達の完了もなければ、ふつうの続行として扱う。

`WAIT` を受けたら、**セッションを閉じずに** `run_task` から抜ける。
「休止中」は新しい状態ファイルを持たず、セッションストアと `BackgroundTasks` から判断する:
閉じていない自律セッションがあり、`has_pending_or_inbox` が真。

- `next_task` は休止中のタスクを、受け箱が空の間は飛ばす。空でなくなったら、
  新しいセッションを作らず、そのセッションを再開する。優先度の順は今までどおり。
- 使ったターン数は履歴から数える。生のログ（`load_session_full`）から読むので、コンテキスト圧縮を越えても保たれる。休止中はターンを回さないので `max_turns` を消費しない
  （`WAIT` と答えたそのターンは数える）。
- セッションストアに書けないとログからの数が進まないので、自律ループはこのプロセスで回したターン数を
  メモリにも持ち、多い方を使う（セッションを閉じたら捨てる）。
- 再起動後は保留が `Interrupted` として受け箱に入るので、休止中のタスクはそのまま再開できる。
- 「他のセッションが動いている」の判定（`is_idle` / `busy_rows`）は自律ストアを読まないので、
  休止中のセッションがあっても自律ループは止まらない。変更は要らない。
- 休止中のタスクの子は、他の自律タスクが動いている間も並行して走る。

## 設定

```toml
[tools.background]
max_per_session = 4   # 0 = バックグラウンド実行を無効にする
```

`[tools.subagent].turn_timeout_secs` はバックグラウンドの子にも効く。
`config.example.toml` の説明コメントを更新する。

## エラー

モデルに返すメッセージには、次に何をすればよいかを入れる。

| 場面 | 返すこと |
|---|---|
| 上限を超えた | 実行中の id をすべて挙げ、`background_task` の `cancel` を案内する |
| セッションのないターン、またはサブエージェントの中から | 「バックグラウンドはメインの会話からだけ使える。同期で呼ぶこと」 |
| 存在しない id の `cancel` | 実行中の id 一覧 |
| 保存ファイルの読み書き失敗 | モデルには返さない。`warn` のみ |

## テスト

`ScriptedProvider` と `BlockingTool` を使い、決まった順序で動かす。

1. `background: true` ですぐに結果が返り、子は親のターンが終わった後も動き続け、
   完了が次のターンの先頭に差し込まれる。
2. 親のターンの実行中に子が終わると、ラウンドの合間のツール結果メッセージに差し込まれ、
   tool_use / tool_result の対が保たれる。
3. 保存済みの答えのない ACP の `Ask` は尋ねずに拒否され、`RefusedCall` として完了に載る。
   「常に許可」のツールは実行され、「常に拒否」のツールは拒否されるが載らない。
4. 上限、深さ 0 以外、セッションなしはどれも断る。
5. 保存ファイルから起動すると、pending が handle つきの `Interrupted` になる。
6. 自律セッション: `WAIT` で休止し、他のタスクが動き、完了が届くと同じセッションで再開する。
   休止中は `max_turns` を消費しない。`DONE` で保留が止まる。
   保留がないのに `WAIT` と言えばふつうの続行になる。
7. 親のターンを取り消してもバックグラウンドは止まらない。セッションを閉じると止まる。

## 範囲外

- **D**: Discord への相談（返信による対応付け、`TaskKind::Question`、待ち時間の上限）。
  投稿したメッセージの ID をチャンネルから受け取る必要がある（今の `send` が返すかは D で確認）。
- ホスト側 `shell` のバックグラウンド化。この層の上に載せる。
- Matrix / Discord 経路での `subagent`（今は `TurnContext` がない）。

## コミットの scope

`feat(tools)` / `feat(autonomous)`。どちらもエージェント内部の scope なので `cliff.toml` は変えない。
