# デスクトップメイト 1/3：プロトコルとサーバー側 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `APP_API` 3 として、`chat.send` のクライアント能力申告・画像添付、表情・アクションのツールと通知、`avatar/` のモデルデータを読み書きする IPC を、core と cli に実装する。

**Architecture:** プロトコルの型・定数・クライアントは `sapphire-agent-core` に置き、サーバー（`cli/`）と GUI が同じ型を使う。表情・アクションは通常の `Tool` として常時登録し、`visible_tool_predicate` で既定では隠し、`run_llm_turn` が `TurnHost::avatar()` を見て出し分け、enum をそのターンの申告で書き換える。実行時は既存の task-local の型（`scope_acp_client` と同じ）でターンの申告と通知先をツールへ渡す。モデルデータは `cli/src/avatar_store.rs`（ファイル操作だけ）と `cli/src/serve/avatar_ipc.rs`（IPC）に分ける。

**Tech Stack:** Rust 2024、tokio、serde / serde_json、toml、base64 0.23、sha2 0.11、sapphire-framework ipc 0.22。

**Spec:** `docs/superpowers/specs/2026-10-28-desktop-companion-design.md`（旧名 desktop-mate-design.md）（§6、§7、§10、§12 の形）

**後続の計画:** 2/3 GUI ライブラリ化（fork、`sapphire-agent-gui`、`Settings`）、3/3 アバター表示とメイトウィンドウ。この計画の完了時点で、CLI の `chat` と既存の desktop は新しい API で今までどおり動く。

## Global Constraints

- `APP_API` は 2 → **3**。doc の変更履歴に「3: `chat.send` に `client`（`avatar`）と `attachments`、`chat.progress` に `expression` / `action`、`sapphire-agent.avatar.*` を追加」。
- 表情の値は申告された一覧から選ぶ。CLI の `chat` は `ClientCaps::default()`（何も申告しない）。
- `set_expression(expression, intensity?: 0.0–1.0 = 1.0)`、説明文「返答の気持ちに合わせて呼ぶ。1 ターンに何度呼んでもよい」。
- `play_action(action)`、説明文「ジェスチャーを一度だけ再生する。挨拶・喜びなど、ここぞという場面でだけ使う」。**1 ターン 1 回まで**。
- 添付: mime は `image/png` / `image/jpeg` / `image/webp`、1 件 20 MiB（デコード後）、1 ターン 8 件まで。違反は `send` のエラーでターンを始めない。
- avatar のパスは `avatar/` からの相対、`/` 区切り。`..`・絶対パス・`avatar.toml` 直接は拒否。1 ファイル 256 MiB、`read` の `len` は 1 MiB まで。
- サーバーは `client` の知らないキーを無視する（`#[serde(deny_unknown_fields)]` を付けない）。
- コミットのスコープ: core の変更は `(rpc)`、cli の変更は `(chat)` / `(tools)`（CLAUDE.md の表）。各コミット末尾に `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。
- Windows で動くこと: パスの表示は `workspace::slash_path`、パス検証は `/` 区切りの文字列で行い `Path::components` に頼らない（`\` も拒否する）。

## Review Focus

1. **申告なしのターン（CLI の `chat`、Discord / Matrix、heartbeat）に avatar ツールが出ないこと** — 期待: ツール一覧に `set_expression` / `play_action` が無い。Task 3 で `agent.rs` の経路（`visible_tool_predicate` を直接使う側）も含めてテストする。
2. **モデルが申告にない名前や範囲外の intensity を渡す** — 期待: ツールエラーで有効な一覧を返し、通知は送らない。Task 3。
3. **アップロード途中の放置・ハッシュ不一致・同じパスへの並行アップロード** — 期待: `avatar/` 配下のファイルは commit まで一切変わらず、不一致なら一時ファイルが消える。Task 4。
4. **`avatar.toml` が参照しているファイルの削除、存在しないファイルを参照する `set_config`** — 期待: どちらも拒否。Task 4。
5. **`avatar/` も `avatar.toml` も無いワークスペース** — 期待: `manifest` は空（`model: None`、`actions` 空、`files` 空）を返し、エラーにしない。Task 4。

## 設計書との差分（この計画で設計書も直す）

- §7「放置されたアップロードは接続が切れたら捨てる」→ サーバー側の `PeerHandle` に切断の通知が無いため、**最後の書き込みから 10 分**で捨てる（次の `write_begin` のときに掃除する）。Task 4 で設計書を直す。
- §7「索引に入らないことを確かめる」→ framework の indexer は markdown 系（`md` / `markdown` / `txt` / `rst` / `org`）と画像拡張子しか索引しないので、`.vrm` / `.vrma` / `.toml` は入らない。除外の実装は不要。Task 4 で設計書に結論を書く。

---

### Task 1: core のプロトコル型と `send` の新しい形

**Files:**
- Modify: `crates/sapphire-agent-core/src/lib.rs`
- Modify: `crates/sapphire-agent-core/Cargo.toml`（`serde = { version = "1.0", features = ["derive"] }`、`base64 = "0.23"` を追加。どちらも cli と同じ版）
- Modify: `cli/src/chat.rs:144-170`（呼び出し側）
- Modify: `desktop/src/bridge.rs:130-145`（呼び出し側）

**Interfaces:**
- Produces（以降のタスクと計画 2/3 が使う）:
  ```rust
  pub const APP_API: u32 = 3;
  pub const KIND_TOOL_START: &str = "tool_start";
  pub const KIND_TOOL_END: &str = "tool_end";
  pub const KIND_EXPRESSION: &str = "expression";
  pub const KIND_ACTION: &str = "action";

  #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
  pub struct ClientCaps {
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub avatar: Option<AvatarCaps>,
  }
  #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
  pub struct AvatarCaps {
      pub expressions: Vec<String>,
      #[serde(default)]
      pub actions: Vec<String>,
  }
  /// `data` は base64。ワイヤ形式そのもの。
  #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
  pub struct Attachment { pub mime: String, pub data: String }
  impl Attachment { pub fn from_bytes(mime: impl Into<String>, bytes: &[u8]) -> Self; }

  pub struct Turn<'a> {
      pub content: &'a str,
      pub attachments: &'a [Attachment],
      pub client: &'a ClientCaps,
  }
  impl<'a> Turn<'a> { pub fn text(content: &'a str) -> Turn<'a>; } // 添付なし・申告なし

  #[non_exhaustive]
  #[derive(Clone, Debug, PartialEq)]
  pub enum ChatEvent {
      Tool(ToolEvent),
      Expression { name: String, intensity: f32 },
      Action { name: String },
  }

  impl ChatClient {
      pub async fn send(&self, session_id: &str, turn: Turn<'_>,
                        on_event: impl FnMut(ChatEvent)) -> Result<String>;
  }

  /// `PROGRESS` の params を解釈する。別セッション・知らない kind は None。
  pub fn parse_progress(session_id: &str, params: &Value) -> Option<ChatEvent>;
  ```
  `ToolEvent` に `#[derive(PartialEq)]` を足す。`Turn::text` 用に空のスライスと `ClientCaps` の static（`static NONE: ClientCaps = ClientCaps { avatar: None };`）を使う。
- `send` が送る params: `{"session_id", "content", "attachments"?, "client"?}`。添付が空なら `attachments` を、`client` が default なら `client` を省く（古い形と同じ JSON になる）。

- [ ] **Step 1: core に失敗するテストを書く**（`lib.rs` 末尾に `#[cfg(test)] mod tests`）

```rust
#[test]
fn progress_kinds_parse_into_events() {
    let s = "s1";
    assert_eq!(
        parse_progress(s, &json!({"session_id":"s1","kind":"tool_start","id":"c1","name":"risky"})),
        Some(ChatEvent::Tool(ToolEvent { started: true, id: "c1".into(), name: "risky".into() }))
    );
    assert_eq!(
        parse_progress(s, &json!({"session_id":"s1","kind":"expression","expression":"happy","intensity":0.5})),
        Some(ChatEvent::Expression { name: "happy".into(), intensity: 0.5 })
    );
    assert_eq!(
        parse_progress(s, &json!({"session_id":"s1","kind":"action","action":"peace"})),
        Some(ChatEvent::Action { name: "peace".into() })
    );
}

#[test]
fn progress_for_another_session_or_kind_is_ignored() {
    assert_eq!(parse_progress("s1", &json!({"session_id":"s2","kind":"action","action":"peace"})), None);
    assert_eq!(parse_progress("s1", &json!({"session_id":"s1","kind":"audio"})), None);
}

#[test]
fn an_expression_without_intensity_is_full() {
    assert_eq!(
        parse_progress("s1", &json!({"session_id":"s1","kind":"expression","expression":"sad"})),
        Some(ChatEvent::Expression { name: "sad".into(), intensity: 1.0 })
    );
}

#[test]
fn a_plain_turn_serialises_like_api_2() {
    assert_eq!(send_params("s1", &Turn::text("hi")), json!({"session_id":"s1","content":"hi"}));
}
```
`send_params(session_id: &str, turn: &Turn) -> Value` は `send` が送る JSON を作る crate 内関数（`pub(crate)`）。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p sapphire-agent-core`
Expected: FAIL（`parse_progress` / `ChatEvent` / `Turn` / `send_params` が未定義）

- [ ] **Step 3: 型・`parse_progress`・`send_params`・新しい `send` を実装し、`APP_API` を 3 にする**

`send` の内側のクロージャは `parse_progress` を呼ぶだけにする。モジュール doc の `SEND` / `PROGRESS` の説明を新しい形に直し、`APP_API` の doc に変更履歴の行を足す。

- [ ] **Step 4: 呼び出し側を直す**

`cli/src/chat.rs`: `client.send(session_id, Turn::text(content), |event| if let ChatEvent::Tool(e) = event { on_tool(e) })` の形にし、表示は今までどおり。`desktop/src/bridge.rs` も同様に `ChatEvent::Tool` だけを `BridgeEvent::ToolStart/ToolEnd` に変換する。`cli/src/serve/chat_ipc.rs` のテスト `tool_calls_are_reported_while_the_turn_runs` を新しい `send` の形に書き換える（期待値は同じ 2 件の `ChatEvent::Tool`）。

- [ ] **Step 5: テストを確認**

Run: `cargo test -p sapphire-agent-core && cargo test -p sapphire-agent chat_ipc && cargo build --workspace`
Expected: すべて PASS、ビルド成功

- [ ] **Step 6: Commit**

```bash
git add crates/sapphire-agent-core cli/src/chat.rs cli/src/serve/chat_ipc.rs desktop/src/bridge.rs Cargo.lock
git commit -m "feat(rpc): declare client capabilities and attachments per turn, and stream expression and action events"
```

---

### Task 2: サーバーが `client` と `attachments` を受け取る

**Files:**
- Modify: `cli/src/serve/chat_ipc.rs`（`SendParams`、`send`、`IpcProgress`）
- Test: `cli/src/serve/chat_ipc.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 1 の `ClientCaps`、`Attachment`、`Turn`、`ChatClient::send`。
- Produces: `IpcProgress` が `client: ClientCaps` を持つ（Task 3 が `TurnHost::avatar()` をここで実装する）。
  ```rust
  #[derive(Deserialize)]
  struct SendParams {
      session_id: String,
      #[serde(default)] content: String,
      #[serde(default)] attachments: Vec<Attachment>,
      #[serde(default)] client: ClientCaps,
  }
  /// 添付を検証して user メッセージを作る。違反は invalid() のメッセージ。
  fn user_message(content: &str, attachments: Vec<Attachment>) -> Result<ChatMessage, String>;
  ```
  定数: `MAX_ATTACHMENTS: usize = 8`、`MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024`、`ATTACHMENT_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp"]`。サイズはデコードして測る（`base64::engine::general_purpose::STANDARD`）。添付があれば `ChatMessage::user_with_images(content, attachments.map(|a| (a.mime, a.data)))`、無ければ `ChatMessage::user(content)`。本文が空で添付も無ければ従来どおり `"nothing to send"`。添付だけなら本文は空のまま送る。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn attachments_become_image_parts() {
    let png = Attachment::from_bytes("image/png", b"\x89PNG....");
    let msg = user_message("look", vec![png.clone()]).unwrap();
    assert!(msg.parts.iter().any(|p| matches!(p, ContentPart::Image { media_type, data_base64 }
        if media_type == "image/png" && *data_base64 == png.data)));
    assert!(msg.parts.iter().any(|p| matches!(p, ContentPart::Text(t) if t == "look")));
}

#[test]
fn attachments_over_the_limits_are_refused() {
    let one = Attachment::from_bytes("image/png", b"x");
    assert!(user_message("", vec![one.clone(); 9]).unwrap_err().contains("8"));
    let big = Attachment::from_bytes("image/png", &vec![0u8; 20 * 1024 * 1024 + 1]);
    assert!(user_message("", vec![big]).is_err());
    let gif = Attachment::from_bytes("image/gif", b"GIF89a");
    assert!(user_message("", vec![gif]).unwrap_err().contains("image/gif"));
    let bad = Attachment { mime: "image/png".into(), data: "not base64!".into() };
    assert!(user_message("", vec![bad]).is_err());
}

#[tokio::test]
async fn an_unknown_client_capability_is_ignored() {
    // a_turn_round_trips_over_ipc と同じ準備で、client.call(SEND, {..., "client": {"future": 1}})
    // を生の JSON で呼び、返答 "hello back" が返ること。
}
```
（`ContentPart` のフィールド名は `cli/src/provider/mod.rs:46` に合わせる。）

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p sapphire-agent chat_ipc`
Expected: FAIL（`user_message` 未定義）

- [ ] **Step 3: `SendParams` を広げ、`user_message` を実装し、`send` で使う。`IpcProgress` に `client: ClientCaps` を持たせる**

- [ ] **Step 4: テストを確認**

Run: `cargo test -p sapphire-agent chat_ipc`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add cli/src/serve/chat_ipc.rs
git commit -m "feat(chat): accept image attachments and client capabilities over IPC"
```

---

### Task 3: `set_expression` / `play_action` と、ターンごとの出し分け

**Files:**
- Create: `cli/src/tools/avatar_tools.rs`
- Modify: `cli/src/tools/mod.rs`（`pub mod avatar_tools;`）
- Modify: `cli/src/serve/mod.rs`（`TurnHost::avatar`、`visible_tool_predicate`、`run_llm_turn` の絞り込み、ツール実行の scope）
- Modify: `cli/src/serve/chat_ipc.rs`（`IpcProgress` の `TurnHost::avatar` と `AvatarSink`）
- Modify: `cli/src/main.rs`（2 つのツールを登録。既存の `register_tool` 群の並び）

**Interfaces:**
- Consumes: Task 1 の `AvatarCaps`、`KIND_EXPRESSION` / `KIND_ACTION`。Task 2 の `IpcProgress.client`。
- Produces:
  ```rust
  // cli/src/tools/avatar_tools.rs
  pub(crate) const SET_EXPRESSION: &str = "set_expression";
  pub(crate) const PLAY_ACTION: &str = "play_action";
  pub(crate) fn is_avatar_tool(name: &str) -> bool;

  #[async_trait]
  pub(crate) trait AvatarSink: Send + Sync {
      async fn expression(&self, name: &str, intensity: f32);
      async fn action(&self, name: &str);
  }
  pub(crate) struct AvatarTurn { /* caps, sink, action_used: AtomicBool */ }
  impl AvatarTurn {
      pub(crate) fn new(caps: AvatarCaps, sink: Arc<dyn AvatarSink>) -> Arc<Self>;
      pub(crate) fn caps(&self) -> &AvatarCaps;
  }
  pub(crate) fn scope_avatar_turn<F: Future>(turn: Option<Arc<AvatarTurn>>, fut: F)
      -> impl Future<Output = F::Output>;
  pub(crate) fn current_avatar_turn() -> Option<Arc<AvatarTurn>>;

  /// ターンのツール一覧に申告を反映する: caps が None なら 2 つを除き、
  /// actions が空なら play_action を除き、残ったものの enum を申告で置き換える。
  pub(crate) fn specialize_specs(specs: &mut Vec<ToolSpec>, caps: Option<&AvatarCaps>);

  pub(crate) struct SetExpressionTool; // kind() = ToolKind::Think
  pub(crate) struct PlayActionTool;    // kind() = ToolKind::Think

  // cli/src/serve/mod.rs の TurnHost に追加（既定 None）
  fn avatar(&self) -> Option<Arc<crate::tools::avatar_tools::AvatarTurn>> { None }
  ```
- 振る舞い:
  - `visible_tool_predicate` は `is_avatar_tool(name)` を常に `false` にする（`agent.rs` のチャンネル経路を含め既定で隠す）。
  - `run_llm_turn` の `specs_filtered` のクロージャは、`is_avatar_tool(name)` なら `avatar.is_some()` を返し、それ以外は従来どおり `base` 以下で判定する。その後 `specialize_specs(&mut tool_specs, avatar.as_ref().map(|a| a.caps()))`。
  - ツール実行の scope（`scope_admin_room_profile` を掛けている所、`cli/src/serve/mod.rs:2766` 付近）で `let fut = avatar_tools::scope_avatar_turn(avatar.clone(), fut);` を足す。`avatar` は `TurnLoop` が持つ `progress.avatar()` から一度取る。
  - `IpcProgress` は `client.avatar` が `Some` のとき、それと自分（`AvatarSink` の実装として `PROGRESS` を `{session_id, kind:"expression", expression, intensity}` / `{session_id, kind:"action", action}` で通知）から `AvatarTurn` を **`send` 1 回につき 1 つ**作って保持し、`avatar()` で返す。
  - `SetExpressionTool::execute`: `current_avatar_turn()` が無ければエラー「このターンにはキャラクターがいない」。名前が `caps.expressions` に無ければ `"unknown expression '<x>'; use one of: neutral, happy, …"`。`intensity` が無ければ 1.0、0.0–1.0 の外ならエラー。成功時は sink に通知して `"ok"`。
  - `PlayActionTool::execute`: 名前の検証は同様。`action_used.swap(true)` が `true` なら `"an action was already played this turn; save gestures for moments that matter"` のエラー。成功時は通知して `"ok"`。

- [ ] **Step 1: 単体テストを書く**（`avatar_tools.rs` の `mod tests`。sink は呼ばれた内容を `Mutex<Vec<String>>` に記録するテスト用実装）

```rust
#[tokio::test]
async fn an_expression_in_the_list_is_sent_and_answers_ok() {
    let (turn, sink) = test_turn(&["neutral", "happy"], &["peace"]);
    let out = scope_avatar_turn(Some(turn), SetExpressionTool.execute(&json!({"expression":"happy","intensity":0.4}))).await.unwrap();
    assert_eq!(out, "ok");
    assert_eq!(sink.events(), ["expression happy 0.4"]);
}

#[tokio::test]
async fn an_unknown_expression_or_intensity_is_refused_without_a_notification() {
    let (turn, sink) = test_turn(&["neutral", "happy"], &[]);
    let err = scope_avatar_turn(Some(turn.clone()), SetExpressionTool.execute(&json!({"expression":"smug"}))).await.unwrap_err();
    assert!(err.to_string().contains("neutral, happy"));
    assert!(scope_avatar_turn(Some(turn), SetExpressionTool.execute(&json!({"expression":"happy","intensity":1.5}))).await.is_err());
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn a_second_action_in_one_turn_is_refused() {
    let (turn, sink) = test_turn(&["neutral"], &["peace", "wave"]);
    scope_avatar_turn(Some(turn.clone()), PlayActionTool.execute(&json!({"action":"peace"}))).await.unwrap();
    let err = scope_avatar_turn(Some(turn), PlayActionTool.execute(&json!({"action":"wave"}))).await.unwrap_err();
    assert!(err.to_string().contains("already"));
    assert_eq!(sink.events(), ["action peace"]);
}

#[tokio::test]
async fn without_an_avatar_turn_the_tools_refuse() {
    assert!(SetExpressionTool.execute(&json!({"expression":"happy"})).await.is_err());
}

#[test]
fn specs_follow_the_declaration() {
    let all = || vec![SetExpressionTool.spec().clone(), PlayActionTool.spec().clone()];
    let mut none = all();
    specialize_specs(&mut none, None);
    assert!(none.is_empty());

    let mut no_actions = all();
    specialize_specs(&mut no_actions, Some(&caps(&["neutral", "happy"], &[])));
    assert_eq!(names(&no_actions), ["set_expression"]);
    assert_eq!(no_actions[0].input_schema["properties"]["expression"]["enum"], json!(["neutral", "happy"]));

    let mut both = all();
    specialize_specs(&mut both, Some(&caps(&["neutral"], &["peace"])));
    assert_eq!(both[1].input_schema["properties"]["action"]["enum"], json!(["peace"]));
}
```

- [ ] **Step 2: 出し分けのテストを書く**（`cli/src/serve/mod.rs` の既存 `tool_names_for_turn_without_a_client` の隣）

```rust
#[tokio::test]
async fn avatar_tools_are_hidden_from_turns_without_a_declaration() {
    // client_filtering_test_set() に SetExpressionTool / PlayActionTool を足した集合で
    // visible_tool_predicate(false, false, false, false, false) と (…, true, true, true, true) の両方を通し、
    // どちらにも "set_expression" / "play_action" が含まれないこと。
}
```
`chat_ipc.rs` の `mod tests` には IPC 経由の結合テストを足す:

```rust
#[tokio::test]
async fn an_expression_tool_call_reaches_the_client_as_an_event() {
    // for_test_scripted_with_log で、1 回目の応答が set_expression {"expression":"happy"}、
    // 2 回目が reply_text("done")。SetExpressionTool を登録。
    // send(Turn { client: &ClientCaps { avatar: Some(caps(["neutral","happy"], [])) }, .. })
    // → events に ChatEvent::Expression { name: "happy", intensity: 1.0 } が含まれ、
    //    log の 1 回目の呼び出しのツール名に "set_expression" があり "play_action" が無いこと。
}

#[tokio::test]
async fn a_turn_without_a_declaration_offers_no_avatar_tools() {
    // Turn::text("hi") で送り、log のツール名に set_expression / play_action が無いこと。
}
```

- [ ] **Step 3: 失敗を確認**

Run: `cargo test -p sapphire-agent avatar_tools && cargo test -p sapphire-agent avatar`
Expected: FAIL（モジュール未定義）

- [ ] **Step 4: `avatar_tools.rs` を実装し、`TurnHost::avatar`、`visible_tool_predicate`、`run_llm_turn`、scope、`IpcProgress`、`main.rs` の登録を入れる**

スキーマの雛形: `{"type":"object","properties":{"expression":{"type":"string","enum":[]},"intensity":{"type":"number","minimum":0,"maximum":1}},"required":["expression"]}`、`play_action` は `{"type":"object","properties":{"action":{"type":"string","enum":[]}},"required":["action"]}`。enum は `specialize_specs` が埋める。

- [ ] **Step 5: テストを確認**

Run: `cargo test -p sapphire-agent`
Expected: PASS（既存テストを含む）

- [ ] **Step 6: Commit**

```bash
git add cli/src/tools/avatar_tools.rs cli/src/tools/mod.rs cli/src/serve/mod.rs cli/src/serve/chat_ipc.rs cli/src/main.rs
git commit -m "feat(tools): let the agent set its avatar's expression and play a gesture on clients that show one"
```

---

### Task 4: `avatar/` のファイルを扱う `AvatarStore`

**Files:**
- Create: `cli/src/avatar_store.rs`
- Modify: `cli/src/main.rs`（`mod avatar_store;`）
- Modify: `crates/sapphire-agent-core/src/lib.rs`（共有のデータ型）
- Modify: `docs/superpowers/specs/2026-10-28-desktop-mate-design.md` §7（「設計書との差分」の 2 点）

**Interfaces:**
- Produces（core に置き、GUI も使う）:
  ```rust
  #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
  pub struct AvatarConfig {
      #[serde(default, skip_serializing_if = "Option::is_none")] pub model: Option<String>,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub idle: Option<String>,
      #[serde(default)] pub actions: std::collections::BTreeMap<String, String>,
  }
  #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
  pub struct AvatarFile { pub path: String, pub size: u64, pub sha256: String }
  #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
  pub struct AvatarManifest {
      #[serde(flatten)] pub config: AvatarConfig,
      pub files: Vec<AvatarFile>,   // avatar.toml を除く全ファイル、path 昇順
  }
  pub const AVATAR_MAX_FILE: u64 = 256 * 1024 * 1024;
  pub const AVATAR_CHUNK: usize = 1024 * 1024;
  ```
- Produces（cli 内）:
  ```rust
  pub(crate) struct AvatarStore { /* dir, staging, uploads: Mutex<HashMap<String, Upload>> */ }
  impl AvatarStore {
      /// dir = <workspace>/avatar、staging = 書き込み途中のファイルを置く所
      pub(crate) fn new(dir: PathBuf, staging: PathBuf) -> Self;
      pub(crate) fn manifest(&self) -> Result<AvatarManifest, AvatarError>;
      pub(crate) fn read(&self, path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, bool), AvatarError>;
      pub(crate) fn write_begin(&self, path: &str, size: u64, sha256: &str) -> Result<String, AvatarError>;
      pub(crate) fn write_chunk(&self, upload_id: &str, offset: u64, data: &[u8]) -> Result<(), AvatarError>;
      pub(crate) fn write_commit(&self, upload_id: &str) -> Result<(), AvatarError>;
      pub(crate) fn set_config(&self, config: &AvatarConfig) -> Result<(), AvatarError>;
      pub(crate) fn remove(&self, path: &str) -> Result<(), AvatarError>;
  }
  pub(crate) enum AvatarError { Invalid(String), Io(std::io::Error) }  // Invalid → INVALID_PARAMS
  pub(crate) fn validate_path(path: &str) -> Result<(), AvatarError>;
  ```
- 振る舞い:
  - `validate_path`: 空、先頭 `/`、`\` を含む、`:` を含む（Windows のドライブ）、`..` や `.` の要素、空要素（`a//b`）、`avatar.toml` そのものは `Invalid`。
  - `manifest`: `dir` が無ければ空の manifest。`avatar.toml` が無ければ `AvatarConfig::default()`。壊れた toml は `Invalid("avatar.toml: …")`。
  - `read`: `len > AVATAR_CHUNK` は `Invalid`。`eof` は `offset + 読んだ量 >= ファイルサイズ`。
  - `write_begin`: `size > AVATAR_MAX_FILE` は `Invalid`。最終書き込みから 10 分経ったアップロードを掃除してから、`staging/<upload_id>` を作る。`upload_id` は `uuid::Uuid::now_v7().to_string()`（cli の既存依存 `uuid` の `v7` feature）。
  - `write_chunk`: `offset` は書き込み済みの長さと一致しなければ `Invalid`（順番どおりのみ）。合計が宣言サイズを超えたら `Invalid` にしてアップロードを捨てる。
  - `write_commit`: 長さと sha256 が宣言と一致しなければ一時ファイルを消して `Invalid`。一致すれば `dir/<path>` の親を作り、`rename`。`rename` が失敗したら（別ファイルシステム）同じディレクトリの `<path>.uploading` にコピーしてから `rename`。
  - `set_config`: `model` / `idle` / `actions` の値がすべて `validate_path` を通り、`dir` に存在すること。違反は `Invalid`。`avatar.toml` は `toml::to_string_pretty` で、一時ファイル → `rename`。
  - `remove`: 今の `avatar.toml` が参照しているパスは `Invalid("… is in use by avatar.toml")`。無いパスは `Invalid`。
- staging の実際の場所（Task 5 で渡す）: `dirs::cache_dir()/sapphire-agent/avatar-uploads`（`image_cache` と同じ流儀）。

- [ ] **Step 1: 失敗するテストを書く**（`tempfile::tempdir()` で `dir` と `staging` を作る）

```rust
#[test] fn an_empty_workspace_has_an_empty_manifest()        // §Review Focus 5
#[test] fn paths_that_escape_or_name_the_config_are_refused() {
    for p in ["", "/abs", "../x", "a/../b", "a//b", "a\\b", "C:x", "avatar.toml", "./a"] {
        assert!(matches!(validate_path(p), Err(AvatarError::Invalid(_))), "{p}");
    }
    assert!(validate_path("actions/peace.vrma").is_ok());
}
#[test] fn an_upload_lands_only_on_commit() {
    // write_begin("model.vrm", 6, sha("abcdef")) → chunk(0,"abc") → dir/model.vrm が無い
    // → chunk(3,"def") → commit → dir/model.vrm == "abcdef"、manifest.files に sha 付きで載る
}
#[test] fn a_hash_mismatch_discards_the_upload()            // §Review Focus 3、staging が空になる
#[test] fn out_of_order_or_oversized_chunks_are_refused()
#[test] fn uploads_idle_for_ten_minutes_are_swept()         // Upload の最終時刻をテストから過去に設定できるよう
                                                            // `sweep(&self, now: Instant)` を pub(crate) にして直接呼ぶ
#[test] fn read_returns_chunks_and_eof()
#[test] fn set_config_needs_existing_files_and_round_trips()// §Review Focus 4
#[test] fn a_file_in_use_cannot_be_removed()                // §Review Focus 4
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p sapphire-agent avatar_store`
Expected: FAIL（モジュール未定義）

- [ ] **Step 3: core の型と `AvatarStore` を実装する**

- [ ] **Step 4: テストを確認**

Run: `cargo test -p sapphire-agent avatar_store && cargo test -p sapphire-agent-core`
Expected: PASS

- [ ] **Step 5: 設計書 §7 を直す**（10 分で掃除する件と、索引に入らない結論）

- [ ] **Step 6: Commit**

```bash
git add cli/src/avatar_store.rs cli/src/main.rs crates/sapphire-agent-core/src/lib.rs docs/superpowers/specs/2026-10-28-desktop-mate-design.md
git commit -m "feat(chat): store the avatar's model and gestures in the workspace's avatar/ folder"
```

---

### Task 5: `sapphire-agent.avatar.*` の IPC とクライアント

**Files:**
- Create: `cli/src/serve/avatar_ipc.rs`
- Modify: `cli/src/serve/mod.rs`（`pub mod avatar_ipc;`）
- Modify: `cli/src/serve/chat_ipc.rs`（`ChatTunnel::router` で avatar のメソッドも登録）
- Modify: `crates/sapphire-agent-core/src/lib.rs`（メソッド名とクライアント）

**Interfaces:**
- Consumes: Task 4 の `AvatarStore`、`AvatarConfig` / `AvatarManifest`、`AVATAR_CHUNK`。
- Produces（core）:
  ```rust
  pub const AVATAR_MANIFEST: &str = "sapphire-agent.avatar.manifest";
  pub const AVATAR_READ: &str = "sapphire-agent.avatar.read";
  pub const AVATAR_WRITE_BEGIN: &str = "sapphire-agent.avatar.write_begin";
  pub const AVATAR_WRITE_CHUNK: &str = "sapphire-agent.avatar.write_chunk";
  pub const AVATAR_WRITE_COMMIT: &str = "sapphire-agent.avatar.write_commit";
  pub const AVATAR_SET_MANIFEST: &str = "sapphire-agent.avatar.set_manifest";
  pub const AVATAR_REMOVE: &str = "sapphire-agent.avatar.remove";

  impl ChatClient {
      pub async fn avatar_manifest(&self) -> Result<AvatarManifest>;
      /// ファイル全体を AVATAR_CHUNK ずつ読む
      pub async fn avatar_download(&self, path: &str) -> Result<Vec<u8>>;
      /// sha256 を計算し begin → chunk… → commit
      pub async fn avatar_upload(&self, path: &str, bytes: &[u8]) -> Result<()>;
      pub async fn avatar_set_config(&self, config: &AvatarConfig) -> Result<()>;
      pub async fn avatar_remove(&self, path: &str) -> Result<()>;
  }
  ```
  core に `sha2 = "0.11"` を足す。ワイヤ形式は設計書 §7 の表どおり（`read` は `{data: base64, eof}`、`write_begin` は `{upload_id}`）。`set_manifest` の params は `AvatarConfig` の JSON。
- Produces（cli）: `pub(crate) struct AvatarTunnel` — `ChatTunnel` と同じ `OnceLock<Arc<ServeState>>` 方式ではなく、`ChatTunnel` が `attach` のときに `AvatarStore::new(state.workspace.dir().join("avatar"), staging)` を作って `OnceLock<Arc<AvatarStore>>` に入れ、`avatar_ipc::register(router, store_cell)` がメソッドを足す。エージェント起動前の呼び出しは chat と同じ「still starting」エラー。`AvatarError::Invalid` は `codes::INVALID_PARAMS`、`Io` は `INTERNAL_ERROR`。ファイル I/O は `tokio::task::spawn_blocking` で行う。

- [ ] **Step 1: 失敗する結合テストを書く**（`avatar_ipc.rs` の `mod tests`、`chat_ipc` の `connected` と同じ in-process 接続。ワークスペースは `for_test_scripted` の state のディレクトリ、staging は tempdir）

```rust
#[tokio::test]
async fn upload_configure_and_download_round_trip() {
    let model = vec![7u8; AVATAR_CHUNK + 10];         // 2 チャンクに割れる大きさ
    chat.avatar_upload("model.vrm", &model).await.unwrap();
    chat.avatar_set_config(&AvatarConfig { model: Some("model.vrm".into()), ..Default::default() }).await.unwrap();
    let manifest = chat.avatar_manifest().await.unwrap();
    assert_eq!(manifest.config.model.as_deref(), Some("model.vrm"));
    assert_eq!(manifest.files[0].size, model.len() as u64);
    assert_eq!(chat.avatar_download("model.vrm").await.unwrap(), model);
}

#[tokio::test]
async fn an_escaping_path_is_an_invalid_params_error() {
    let err = chat.avatar_download("../config.toml").await.unwrap_err();
    assert!(err.to_string().contains("invalid") || err.to_string().contains(".."));
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p sapphire-agent avatar_ipc`
Expected: FAIL

- [ ] **Step 3: core のメソッド名・クライアントと、cli の `avatar_ipc.rs`・登録を実装する**

- [ ] **Step 4: 全体を確認**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: すべて PASS、警告なし

- [ ] **Step 5: Commit**

```bash
git add cli/src/serve/avatar_ipc.rs cli/src/serve/mod.rs cli/src/serve/chat_ipc.rs crates/sapphire-agent-core
git commit -m "feat(chat): serve the avatar's files to local clients over IPC"
```
