# ACP v1 での MCP over ACP 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ACP クライアントが `session/new` / `session/load` で渡す `"type": "acp"` の MCP サーバーを受け付ける。そのツールを `acp__<server>__<tool>` として、そのセッションのターンでだけモデルに見せる。

**Architecture:**
- `agent-client-protocol` を 5.1 に上げ、`unstable_mcp_over_acp` を有効にする。
- rmcp の `Transport` を実装した `AcpMcpTransport` を作る。中身は、ACP の `mcp/message` で MCP の request を運ぶ薄い層にする。
- 既存の `McpClient` をこのトランスポートで動かし、セッションごとの `ToolSet` に入れる。
- `run_llm_turn` は、共有の `ToolSet` とセッションの `ToolSet` を、名前の接頭辞（`acp__`）で振り分けて使う。

**Tech Stack:** Rust、`agent-client-protocol` 5.1（`unstable_mcp_over_acp`）、`rmcp` 3.8（`Transport<RoleClient>`、`ClientLifecycleMode::Discover`）、tokio

**Spec:** `docs/superpowers/specs/2026-11-10-acp-mcp-over-acp-design.md`

## Global Constraints

- **ACP のバージョン**: v1 のまま。`unstable_protocol_v2` は有効にしない。
- **受け付けるトランスポート**: クライアントが渡す MCP サーバーのうち、`acp` 型だけを受け付ける。`stdio` / `http` / `sse` は無視して warn を出す。
- **v1 のクライアント側 fs / terminal の振り分け**（`current_acp_client()`、#270）は変えない。
- **ツール名**: `acp__<serverName>__<tool>`。
  - `serverName` と `tool` は、`[A-Za-z0-9_-]` 以外の文字を `_` に置き換える。
  - 同じセッションの中で `serverName` が重複したら、後のものに `_2`、`_3`… と番号を付ける。
  - 長さは切り詰めない。
- **MCP のプロトコルバージョン**: ACP 経由の MCP は 2026-07-28 に固定する（`ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::LATEST] }`）。`initialize` へのフォールバックはしない。
- **タイムアウト**:
  - `mcp/message` の request は 1 件ごとに 120 秒（`ACP_MCP_REQUEST_TIMEOUT`）。
  - 最初のターンの前に `tools/list` を待つのは最大 10 秒（`CLIENT_MCP_READY_TIMEOUT`）。超えたらツールなしで始める。
- **権限**: annotations から `ToolKind` を決める。
  - `readOnlyHint == Some(true)` → `Read`
  - それ以外で `destructiveHint == Some(false)` → `Edit`
  - それ以外 → `Other`
- **request handler の中で送信した request を await しない**（`serve/acp.rs` の既存の注意どおり）。`tools/list` は、セッションを返したあとに spawn したタスクで取る。
- **subagent にはセッションのツールを渡さない**（spec の範囲外）。
- **コミットの scope**: SDK の更新は `deps`、それ以外は `acp` または `mcp`。メッセージの末尾に `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` を付ける。
- **テストの実行**: `cargo test -p sapphire-agent-server`。最後に `cargo test --workspace` と `cargo clippy --workspace --all-targets`。

## Review Focus

1. **ターンの途中で ACP 接続が切れる**: `tools/call` の待ちは「接続が閉じた」エラーで終わり、ターンは止まらずにツールのエラーとして続く。120 秒待たされてはいけない。→ Task 2 の `pending_requests_fail_when_the_peer_closes` と、Task 5 の `closing_the_connection_drops_the_session_tools`。
2. **セッションのツールが、別のセッションや別の接続から見える・呼べる**: 見えてはいけない。名前を推測して `acp__…` を呼んでも、`Unknown tool` になる。→ Task 4 の `session_tools_are_invisible_without_a_session_tool_set`。
3. **`tools/list` が来ないまま最初のプロンプトが来る**: 最大 10 秒待ち、その後はツールなしでターンを進める。プロンプトの応答を止めてはいけない。→ Task 5 の `a_prompt_does_not_wait_past_the_ready_timeout`。
4. **クライアントが `mcp/message` を実装していない（method not found）**: セッションは成り立ち、warn が 1 回出るだけで済む。→ Task 5 の `a_client_without_mcp_message_still_gets_a_session`。
5. **`session/load` で同じセッションを別の接続から開き直す**: 古い接続のツールは捨て、新しく渡された `mcpServers` で作り直す。→ Task 5 の `load_rebuilds_session_tools_from_the_new_request`。

---

## File Structure

| ファイル | 役割 |
|---|---|
| `crates/sapphire-agent-server/Cargo.toml` | SDK 5.1 と `unstable_mcp_over_acp` |
| `crates/sapphire-agent-server/src/mcp_client/acp_transport.rs`（新規） | `McpOverAcpPeer` trait と `AcpMcpTransport`。ACP の型は知らず、JSON の `Value` だけを扱う |
| `crates/sapphire-agent-server/src/mcp_client/mod.rs` | `McpClient` が ACP トランスポートでも動くようにする。annotations から種別を決める。名前の接頭辞 |
| `crates/sapphire-agent-server/src/tools/mod.rs` | `refresh_client_tools` の接頭辞を `McpClient` から取るようにする |
| `crates/sapphire-agent-server/src/serve/acp_mcp.rs`（新規） | `ConnectionTo<Client>` の上の `McpOverAcpPeer` 実装、受信した notification の振り分け、名前の整形、セッションのツールの組み立て |
| `crates/sapphire-agent-server/src/serve/acp.rs` | SDK 5 への追従、`initialize` での能力の宣言、`session/new` / `load` の配線、`mcp/message` notification の受け口、`AcpSession` にセッションのツールを持たせる |
| `crates/sapphire-agent-server/src/serve/mod.rs` | `TurnHost::session_tools`。一覧・種別・実行で、共有の `ToolSet` とセッションの `ToolSet` を合わせる |
| `docs/sapphire-agent-tools.md`、`crates/sapphire-agent-server/config.example.toml` | ドキュメント |

---

### Task 1: `agent-client-protocol` を 5.1 に上げる（挙動は変えない）

**Files:**
- Modify: `crates/sapphire-agent-server/Cargo.toml:200-204`
- Modify: `crates/sapphire-agent-server/src/serve/acp.rs`、`crates/sapphire-agent-server/src/serve/acp_tunnel.rs`（コンパイルが通るまで）
- Modify: ワークスペースの中で同じ crate を使っている他の場所（`desktop/`、`crates/sapphire-agent-core/` に依存があればそれも）

**Interfaces:**
- Produces: `agent_client_protocol` 5.1。v1 の型は `agent_client_protocol::v1::schema::*` から参照する。`McpServer::Acp(McpServerAcp)`、`MessageMcpRequest`、`MessageMcpResponse`、`MessageMcpNotification`、`McpCapabilities::acp(bool)` が使える状態にする。

- [ ] **Step 1: 依存を書き換える**

```toml
agent-client-protocol = { version = "5.1", features = ["unstable_mcp_over_acp"] }
```

  - コメントの「Default features only — `unstable_protocol_v2` stays off」は、`unstable_mcp_over_acp` だけを有効にしたことがわかるように直す。
  - `rg 'agent-client-protocol' --glob Cargo.toml` で、ワークスペースの中の他の依存も確認する。

- [ ] **Step 2: `cargo build -p sapphire-agent-server` を通す**

  - 移行の手引きは、crate の `CHANGELOG.md` 5.0.0 の節と、crate に付いている v5 の migration guide（#472）。
  - 主な追従は次のとおり。
    - import を `v1::schema` 経由にする。
    - `Agent.builder()` を、5.0 のバージョンに依存しない `Builder::new` / v1 の接続型にする。
    - SDK の `Error` の変換。
    - request handle の ID の型。
  - 挙動を変える書き換えはしない。

- [ ] **Step 3: 既存のテストを通す**

Run: `cargo test -p sapphire-agent-server`
Expected: すべて PASS（テストの数は更新前と同じ）。

- [ ] **Step 4: コミットする**

```bash
git add Cargo.lock crates/sapphire-agent-server/Cargo.toml crates/sapphire-agent-server/src
git commit -m "fix(deps): update agent-client-protocol to 5.1 with unstable_mcp_over_acp"
```

---

### Task 2: `AcpMcpTransport`（rmcp の `Transport` を `mcp/message` で運ぶ）

最初に spike の判定をする。Step 1 から Step 3 で、rmcp の `Discover` ハンドシェイクと `tools/list` がこのトランスポートで通らなければ、ここで止めて報告する（spec §2 のリスク）。

**Files:**
- Create: `crates/sapphire-agent-server/src/mcp_client/acp_transport.rs`（テストは同じファイルの `#[cfg(test)] mod tests`）
- Modify: `crates/sapphire-agent-server/src/mcp_client/mod.rs`（`pub mod acp_transport;`）

**Interfaces:**
- Produces:

```rust
/// One MCP-over-ACP binding: the ACP connection, seen only as JSON.
#[async_trait::async_trait]
pub trait McpOverAcpPeer: Send + Sync + 'static {
    /// Send one `mcp/message` request and wait for its outer response.
    /// `Ok(McpCarrier)` is the successful outer response; `Err` is an outer
    /// ACP error or a closed connection. Dropping the future cancels the
    /// outer request (`$/cancel_request`).
    async fn request(&self, server_id: &str, request_id: &str, method: &str, params: serde_json::Value)
        -> Result<McpCarrier, McpOverAcpError>;
    /// Notifications the client sends for `request_id` while it is active.
    fn notifications(&self, server_id: &str, request_id: &str)
        -> tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)>; // (method, params)
    fn is_closed(&self) -> bool;
}

pub enum McpCarrier { Result(serde_json::Value), Error { code: i64, message: String, data: Option<serde_json::Value> } }

#[derive(Debug, thiserror::Error)]
pub enum McpOverAcpError {
    #[error("ACP connection closed")] Closed,
    #[error("ACP error {code}: {message}")] Outer { code: i64, message: String },
    #[error("mcp/message timed out after {0:?}")] Timeout(std::time::Duration),
}

pub const ACP_MCP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct AcpMcpTransport { /* peer: Arc<dyn McpOverAcpPeer>, server_id: String, inbox: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>, ... */ }
impl AcpMcpTransport { pub fn new(peer: Arc<dyn McpOverAcpPeer>, server_id: impl Into<String>) -> Self; }
impl rmcp::transport::Transport<rmcp::RoleClient> for AcpMcpTransport { type Error = McpOverAcpError; /* send / receive / close */ }
```

- **`send`**:
  - **request の場合**: `requestId` を `format!("mcp-request:{}", uuid::Uuid::now_v7())` で発番する。task を spawn し、その中で次を行う。
    - `peer.request` を `ACP_MCP_REQUEST_TIMEOUT` 付きで待つ。
    - 結果を JSON-RPC の response / error（rmcp の元の request id を付ける）にして、inbox に積む。
    - 待っている間は `peer.notifications(...)` を読み、受け取ったものを JSON-RPC の notification として inbox に積む。
    - 外側のエラーとタイムアウトは、JSON-RPC のエラー（code `-32603`、message にエラーの文字列）として返す。
  - **`notifications/cancelled` の場合**: 該当する request の task を abort する。future が drop されることで、外側に `$/cancel_request` が送られる。
  - **それ以外の notification の場合**: 捨てて debug ログを出す。
  - `peer.is_closed()` なら `Err(Closed)` を返す。
- **`receive`**: inbox から取り出す。peer が閉じていて、保留中の request がなく、inbox も空なら `None` を返す。
- **`close`**: 保留中の task をすべて abort し、inbox を閉じる。

- [ ] **Step 1: spike のテストを書く**

`FakePeer` を作る。`server/discover` には 2026-07-28 を名乗る結果、`tools/list` には `{"tools":[{"name":"poke","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}` を返す。受けた `(method, params)` は記録する。

```rust
#[tokio::test]
async fn rmcp_discovers_and_lists_tools_over_mcp_message() {
    let peer = FakePeer::new();
    let svc = ().serve_with_lifecycle(AcpMcpTransport::new(peer.clone(), "game:1"),
        ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::LATEST] }).await.unwrap();
    let tools = svc.list_all_tools().await.unwrap();
    assert_eq!(tools[0].name, "poke");
    assert!(peer.seen().iter().any(|(sid, m)| sid == "game:1" && m == "tools/list"));
}
```

`()` が `ClientHandler` でなければ、テスト用の最小の handler を使う。

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p sapphire-agent-server acp_transport`
Expected: コンパイルエラー（`AcpMcpTransport` が未定義）

- [ ] **Step 3: `AcpMcpTransport` を実装し、spike のテストを通す**

  - Run: `cargo test -p sapphire-agent-server rmcp_discovers_and_lists_tools_over_mcp_message`
  - Expected: PASS
  - rmcp の都合（`server/discover` 以外の前提がある、request を並行に送る順序に制約がある、など）で通らなければ、ここで止めて報告する。

- [ ] **Step 4: 残りのテストを書く**（同じ `FakePeer`）

  - `carrier_error_becomes_an_mcp_error`: `McpCarrier::Error { code: -33002, .. }` → `call_tool` が `ServiceError::McpError` で、code が `-33002`。
  - `outer_error_becomes_an_internal_error`: `Err(Outer { code: -32601, .. })` → code `-32603`、message に `-32601` を含む。
  - `notifications_are_routed_by_request_id`: `tools/call` の待ち中に、同じ `requestId` へ `notifications/progress` を流すと rmcp に届く。別の `requestId` に流したものは届かない。
  - `cancel_aborts_the_outer_request`: `call_tool` の future を drop すると、`FakePeer` が「request の future が drop された」ことを記録する。
  - `pending_requests_fail_when_the_peer_closes`: 待ちの途中で peer を閉じると、`call_tool` が 1 秒以内に `Err` で返る。
  - `a_request_times_out`: tokio の `start_paused` で時間を進め、120 秒で `Timeout` 由来のエラーになる。

- [ ] **Step 5: 実装を仕上げてテストを通す**

Run: `cargo test -p sapphire-agent-server acp_transport`
Expected: 7 件 PASS

- [ ] **Step 6: コミットする**

```bash
git add crates/sapphire-agent-server/src/mcp_client
git commit -m "feat(mcp): an rmcp transport that carries MCP over ACP mcp/message"
```

---

### Task 3: `McpClient` を ACP トランスポートで動かす、annotations から種別を決める、名前の接頭辞

**Files:**
- Modify: `crates/sapphire-agent-server/src/mcp_client/mod.rs`
- Modify: `crates/sapphire-agent-server/src/tools/mod.rs:212-231`（`refresh_client_tools`）
- Test: `crates/sapphire-agent-server/src/mcp_client/tests.rs`

**Interfaces:**
- Consumes: Task 2 の `McpOverAcpPeer`、`AcpMcpTransport::new`
- Produces:

```rust
impl McpClient {
    /// A client for a server the ACP client provides. Never respawns its
    /// transport once the peer is closed: `open` fails with "ACP connection closed".
    pub fn over_acp(name: &str, server_id: &str, peer: Arc<dyn McpOverAcpPeer>, workspace_root: &str) -> Self;
    /// "mcp__<name>__" for configured servers, "acp__<name>__" for ACP-provided ones.
    pub fn tool_prefix(&self) -> String;
}
pub struct RemoteToolSpec { pub name: String, pub description: String, pub input_schema: Value,
    pub annotations: Option<rmcp::model::ToolAnnotations> }
fn kind_for_annotations(a: Option<&rmcp::model::ToolAnnotations>) -> ToolKind;
```

- `McpClient` に `source: McpSource` を持たせる。
  - `enum McpSource { Config(McpServerConfig), Acp { server_id: String, peer: Arc<dyn McpOverAcpPeer> } }`
  - `trust()` は、`Config` なら設定の値、`Acp` なら `McpTrust::None` を返す（使われない）。
- **`handshake`**: `Acp` なら、`AcpMcpTransport` で `ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::LATEST] }` を使う。`open()` の `Auto` とフォールバックは `Config` のときだけ。`peer.is_closed()` なら `anyhow!("ACP connection closed")`。
- **`build_tools_for_client`**:
  - 名前は `format!("{}{}", client.tool_prefix(), rt.name)` にする。
  - 種別は、`Config` なら `kind_for_trust`、`Acp` なら `kind_for_annotations(rt.annotations.as_ref())`。
  - 名前の整形（Global Constraints）は Task 5 でサーバー名に対して行う。ここでのツール名 `rt.name` も同じ規則で整形する。そのための `pub(crate) fn sanitize_tool_segment(s: &str) -> String` をこの Task で `mcp_client/mod.rs` に作る。
- **`refresh_client_tools`**: `format!("mcp__{}__", …)` を `client.tool_prefix()` に置き換える。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn annotations_choose_the_tool_kind() {
    use rmcp::model::ToolAnnotations as A;
    let ro = A { read_only_hint: Some(true), ..Default::default() };
    let nd = A { destructive_hint: Some(false), ..Default::default() };
    let ro_and_destructive = A { read_only_hint: Some(true), destructive_hint: Some(true), ..Default::default() };
    let d  = A { destructive_hint: Some(true), ..Default::default() };
    assert_eq!(kind_for_annotations(Some(&ro)), ToolKind::Read);
    assert_eq!(kind_for_annotations(Some(&ro_and_destructive)), ToolKind::Read);
    assert_eq!(kind_for_annotations(Some(&nd)), ToolKind::Edit);
    assert_eq!(kind_for_annotations(Some(&d)), ToolKind::Other);
    assert_eq!(kind_for_annotations(Some(&A::default())), ToolKind::Other);
    assert_eq!(kind_for_annotations(None), ToolKind::Other);
}
#[test] fn tool_segments_are_sanitized() {
    assert_eq!(sanitize_tool_segment("my game.v2/tools"), "my_game_v2_tools");
    assert_eq!(sanitize_tool_segment("ok-name_1"), "ok-name_1");
}
#[tokio::test] async fn acp_client_tools_are_prefixed_and_classified() {
    // Task 2 の FakePeer（`pub(crate)` にして共有する）で McpClient::over_acp("game", "game:1", ...) を作り、
    // list_tools → build_tools_for_client
    // assert: spec.name == "acp__game__poke", kind() == ToolKind::Read
}
#[tokio::test] async fn acp_client_does_not_reopen_after_the_peer_closes() {
    // peer を閉じたあとの list_tools が Err で、メッセージに "ACP connection closed" を含む
}
```

`ToolAnnotations` に `Default` がなければ、rmcp のコンストラクタを使う。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p sapphire-agent-server mcp_client`
Expected: コンパイルエラー

- [ ] **Step 3: 上の Interfaces どおりに実装する**

`list_tools` で `t.annotations` を `RemoteToolSpec.annotations` に入れる。

- [ ] **Step 4: テストを通す（既存の `mcp_client` / `tools` のテストも含む）**

Run: `cargo test -p sapphire-agent-server mcp_client tools::`
Expected: PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/sapphire-agent-server/src/mcp_client crates/sapphire-agent-server/src/tools/mod.rs
git commit -m "feat(mcp): run McpClient over ACP and classify its tools by annotations"
```

---

### Task 4: ターンでセッションのツールを使う（`TurnHost::session_tools`）

**Files:**
- Modify: `crates/sapphire-agent-server/src/serve/mod.rs`
  - `TurnHost`（1912 行付近）
  - `run_llm_turn` のツール一覧の組み立て（3325-3375 行付近）
  - `TurnLoop` の種別の参照（2820 行付近）と実行（2904 / 2950 行付近）
- Test: `crates/sapphire-agent-server/src/serve/mod.rs` の `mod tests`

**Interfaces:**
- Produces:

```rust
// TurnHost
/// Tools the ACP client provided for this session (`acp__…`). `None` everywhere else.
fn session_tools(&self) -> Option<Arc<crate::tools::ToolSet>> { None }

pub(crate) const SESSION_TOOL_PREFIX: &str = "acp__";
```

- **ツール一覧**: `tool_specs` の後ろに、`session_tools.specs_filtered(|n| n.starts_with(SESSION_TOOL_PREFIX))` を足す。その前に `session_tools.refresh_if_needed()` を呼ぶ。`visible_tool_predicate` は共有のツールにだけかける。
- **種別**: `kinds` は、共有の `kinds()` にセッションの `kinds()` を連結したものにする。
- **実行**: 名前が `SESSION_TOOL_PREFIX` で始まり、`session_tools` が `Some` ならそちらの `execute`、そうでなければ共有の `execute` に渡す。共有の `ToolSet` に `acp__` の名前は存在しないので、セッションのツールがないときは今の `Unknown tool` がそのまま返る。
- subagent の `TurnLoop`（`subagent_depth > 0`）は `session_tools` を見ない。

- [ ] **Step 1: 失敗するテストを書く**

テストは `ServeState::for_test_scripted` と、`TurnHost` のテスト用の実装を使う。既存のテストの host 実装に `session_tools` を足すか、それを包む型を作る。

  - `session_tools_are_offered_and_executed`:
    - セッションの `ToolSet` に、`acp__game__echo` という名前のテスト用ツールを入れる。`kind = Read` で、入力を返すだけのもの。既存の `EchoTool` に名前を渡せればそれを使う。
    - スクリプトの provider は、1 回目にこのツールを呼び、2 回目に `"done"` を返す。
    - assert:
      - provider の 1 回目の呼び出しのツール一覧に `acp__game__echo` が入っている。
      - 履歴にそのツールの結果が残っている。
      - 最後の text が `"done"`。
  - `session_tools_are_invisible_without_a_session_tool_set`:
    - `session_tools` が `None` で、スクリプトが `acp__game__echo` を呼ぶ。
    - assert:
      - ツール一覧に入っていない。
      - 結果が `"Unknown tool: acp__game__echo"`。
  - `session_tool_kind_drives_the_permission_decision`:
    - `Origin::Acp(SessionMode::Default)` で、`kind = Other` のセッションのツールを呼ぶ。
    - assert: 確認が求められる（既存の確認テストと同じ観測方法を使う）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p sapphire-agent-server serve::tests::session_tool`
Expected: FAIL / コンパイルエラー

- [ ] **Step 3: Interfaces どおりに実装する**

- [ ] **Step 4: `serve::tests` 全体を通す**

Run: `cargo test -p sapphire-agent-server serve::`
Expected: PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/sapphire-agent-server/src/serve/mod.rs
git commit -m "feat(acp): offer and run per-session tools in a turn"
```

---

### Task 5: ACP の配線（能力の宣言、`session/new` / `load`、`mcp/message` の受信、寿命）

**Files:**
- Create: `crates/sapphire-agent-server/src/serve/acp_mcp.rs`（`serve/mod.rs` に `pub mod acp_mcp;`）
- Modify: `crates/sapphire-agent-server/src/serve/acp.rs`
  - `initialize`（1400 行付近の `AgentCapabilities::new()`）
  - `session/new`（1425-1442 行の warn を置き換える）
  - `session/load` / `resume` の `adopt_session`
  - notification の handler（1659 行付近）
  - `AcpSession` / `AcpProgress`
- Test: `serve/acp_mcp.rs` の `mod tests`、`serve/acp.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 2 の `McpOverAcpPeer`、Task 3 の `McpClient::over_acp` / `build_tools_for_client` / `sanitize_tool_segment`、Task 4 の `TurnHost::session_tools`
- Produces:

```rust
// serve/acp_mcp.rs
pub(crate) const CLIENT_MCP_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The ACP connection as an `McpOverAcpPeer`. One per connection, shared by its sessions.
pub(crate) struct AcpMcpPeer { /* connection: ConnectionTo<Client>, routes: Mutex<HashMap<(String,String), UnboundedSender<(String,Value)>>>, closed: AtomicBool */ }
impl AcpMcpPeer {
    pub(crate) fn new(connection: ConnectionTo<Client>) -> Arc<Self>;
    /// Route one `mcp/message` notification from the client. Unknown (server_id, request_id): dropped, debug log.
    pub(crate) fn deliver(&self, notif: MessageMcpNotification);
    /// Called from connection teardown.
    pub(crate) fn close(&self);
}

/// Per-session client tools, filled in the background.
pub(crate) struct ClientMcp { /* tools: Arc<ToolSet>, ready: watch::Receiver<bool> */ }
impl ClientMcp {
    /// Pick the `acp` servers out of `servers`, warn about the rest, name them
    /// (sanitize + `_2`, `_3` on collision), and spawn the `tools/list` of each.
    pub(crate) fn start(servers: &[McpServer], peer: Arc<AcpMcpPeer>, workspace_root: &str, session_id: &str) -> Self;
    /// The tool set once ready, or after CLIENT_MCP_READY_TIMEOUT with whatever arrived.
    pub(crate) async fn tools(&self) -> Arc<ToolSet>;
}
pub(crate) fn client_server_names(names: &[&str]) -> Vec<String>; // sanitize + dedupe, order kept
```

- **`initialize`**: `AgentCapabilities` の `mcp_capabilities` に `.acp(true)` を足す。
- **`session/new` / `adopt_session`**:
  - `ClientMcp::start(&req.mcp_servers, peer, …)` を呼んで `AcpSession` に入れる。`load` のときは、古い `ClientMcp` を置き換える。
  - `tools/list` は `start` が spawn したタスクで行う。handler の中では await しない。
  - warn の文言は次のとおり。

    ```
    ACP: ignoring {n} non-ACP MCP server(s) offered by the client for session {id}: only MCP over ACP ("type": "acp") is accepted. Configure the agent's own MCP servers in [[tools.mcp_servers]].
    ```

  - method not found で失敗したときの warn は次のとおり。

    ```
    ACP: client MCP server '{name}' for session {id} is unavailable: the client does not implement mcp/message
    ```

- **notification**: `on_receive_notification` で `MessageMcpNotification` を受け、`peer.deliver(notif)` に渡す。
- **`AcpProgress::session_tools`**: `TurnHost::session_tools` は同期の関数なので、ここでは await しない。
  - プロンプトの handler が spawn したターンのタスクの先頭（`AcpProgress` を作る前）で、`client_mcp.tools().await` を待つ。
  - 得た `Arc<ToolSet>` を `AcpProgress` のフィールド `session_tools: Option<Arc<ToolSet>>` に入れ、`session_tools()` はその clone を返す。
  - `detached()` で作る host にも引き継ぐ。
- **寿命**: 接続の後始末（`release_connection_sessions` を呼んでいるところ）で `peer.close()` を呼ぶ。`AcpSessions` が drop されれば `ClientMcp` も一緒に消える。

- [ ] **Step 1: 失敗するテストを書く**

  - `acp_mcp::tests::server_names_are_sanitized_and_deduped`:

    ```rust
    assert_eq!(client_server_names(&["game", "game", "my game", "game"]),
               vec!["game", "game_2", "my_game", "game_3"]);
    ```

  - `acp_mcp::tests::non_acp_servers_are_ignored`:
    - `stdio` と `http` を 1 つずつ、`acp` を 1 つ渡す。
    - assert:
      - `tools/list` が送られるのは `acp` の 1 つだけ。
      - stdio / http の `McpClient` は作られない。
  - `acp_mcp::tests::a_prompt_does_not_wait_past_the_ready_timeout`:
    - `tools/list` に答えない peer を使い、`start_paused` にする。
    - assert: `tools()` が 10 秒で空の `ToolSet` を返す。
  - `acp_mcp::tests::a_client_without_mcp_message_still_gets_a_session`:
    - `Outer { code: -32601 }` を返す peer を使う。
    - assert: `tools()` が空のまま返り、panic しない。
  - `acp::tests::initialize_advertises_mcp_over_acp`:
    - `initialize` の応答の `agentCapabilities.mcpCapabilities.acp == true` を確かめる。
    - 既存の `initialize` のテストと同じ方法で応答を観測する。
  - `acp::tests::load_rebuilds_session_tools_from_the_new_request`:
    - `session/new` で `acp` のサーバーを 1 つ渡し、`session/load` で別の名前を渡す。
    - assert: ツール一覧が、新しい名前のものだけになる。
  - `acp::tests::closing_the_connection_drops_the_session_tools`:
    - `tools/call` の応答を返さないクライアントで、呼び出し中に接続を閉じる。
    - assert: ツールの結果がエラーになり、ターンが完了する（120 秒待たない）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p sapphire-agent-server acp_mcp acp::tests`
Expected: FAIL / コンパイルエラー

- [ ] **Step 3: Interfaces どおりに実装する**

- [ ] **Step 4: テストを通す**

Run: `cargo test -p sapphire-agent-server`
Expected: PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/sapphire-agent-server/src/serve
git commit -m "feat(acp): accept client MCP servers over ACP (mcp/message)"
```

---

### Task 6: SDK の提供側との往復テスト、ドキュメント

**Files:**
- Modify: `crates/sapphire-agent-server/Cargo.toml`（`[dev-dependencies]` に `agent-client-protocol-rmcp = { version = "5.1", features = ["rmcp_3", "unstable_mcp_over_acp"] }`）
- Test: `crates/sapphire-agent-server/src/serve/acp.rs` の `mod tests`（または `serve/acp_mcp.rs`）
- Modify: `docs/sapphire-agent-tools.md`、`crates/sapphire-agent-server/config.example.toml`（378-400 行付近、MCP の説明）

**Interfaces:**
- Consumes: Task 5 までのすべて。`ServeState::for_test_scripted`、`serve_connection`

- [ ] **Step 1: 往復テストを書く: `client_provided_mcp_tool_round_trip`**

  - **クライアント側**: SDK 5.1 の v1 クライアントを使い、`agent-client-protocol-rmcp` の `McpServerBuilder` で `poke` ツールを持つ MCP サーバーを作る。
    - `poke` は `readOnlyHint: true` で、`"poked"` を返す。
    - これを `with_mcp_server` でセッションに付ける。
  - **agent 側**: `ServeState::for_test_scripted` を使う。スクリプトは、1 回目に `acp__<name>__poke` を呼び、2 回目に `"ok"` を返す。
  - **接続**: 2 つを duplex のストリームでつなぎ、`serve_connection` に渡す。
  - **流れ**: `initialize` → `session/new` → `session/prompt`
  - **assert**:
    - provider に渡ったツール一覧に `acp__…__poke` がある。
    - 2 回目の provider の呼び出しで、履歴にツールの結果 `"poked"` がある。
    - プロンプトの応答が `end_turn`。
    - `Read` なので、`session/request_permission` が来ない。
  - このテストの冒頭にコメントを書き、ゲーム側の実装の見本であることを示す。

- [ ] **Step 2: 実行する**

Run: `cargo test -p sapphire-agent-server client_provided_mcp_tool_round_trip`
Expected: PASS。失敗した場合、原因は Task 2 から Task 5 にあるので、そちらを直す。

- [ ] **Step 3: ドキュメントを書く**

  - **`docs/sapphire-agent-tools.md`** に「クライアントが出すツール（`acp__…`）」の節を足す。次の 4 点を書く。
    - 受け付けるのは MCP over ACP だけであること。
    - 名前の付け方（整形と番号付け）。
    - annotations から権限を決める表（spec §4 の表）。
    - そのセッションのターンでだけ見え、subagent には渡らないこと。
  - **`config.example.toml`** の MCP の説明に、次の 1 行を足す。

    ```
    MCP servers an ACP client provides over ACP need no entry here; see docs/sapphire-agent-tools.md.
    ```

- [ ] **Step 4: 全体を確かめる**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: すべて成功

- [ ] **Step 5: コミットする**

```bash
git add crates/sapphire-agent-server docs/sapphire-agent-tools.md
git commit -m "test(acp): round-trip a client-provided MCP tool; document acp__ tools"
```

（scope が `test` だと changelog から外れる。ドキュメントは機能のコミットに含めたい場合、Step 3 だけを `docs(acp):` として分けてもよい。）
