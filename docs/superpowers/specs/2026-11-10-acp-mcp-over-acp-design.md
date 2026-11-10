# ACP v1 での MCP over ACP（クライアントが渡す MCP サーバーの受け入れ）

- **対象**: `crates/sapphire-agent-server/Cargo.toml`（`agent-client-protocol` 3.3 → 5.1）、
  `crates/sapphire-agent-server/src/serve/acp.rs`（SDK 5 への追従、`mcpServers` の受け入れ）、
  `crates/sapphire-agent-server/src/mcp_client/`（ACP トランスポート、annotations による種別）、
  `crates/sapphire-agent-server/src/serve/mod.rs`（`run_llm_turn` にセッション単位のツールを足す）、
  `docs/sapphire-agent-tools.md`、`crates/sapphire-agent-server/config.example.toml`
- **参照**: [RFD: MCP-over-ACP](https://agentclientprotocol.com/rfds/mcp-over-acp)、
  [ACP session setup](https://agentclientprotocol.com/protocol/session-setup)、
  `agent-client-protocol` 5.1.0 の `unstable_mcp_over_acp`（`v1::schema` の `mcp/message` 型）
- **関連 issue**: #390（ACP v2 への移行。この仕様の範囲外をまとめたもの）
- **状態**: 設計

## なぜ

ACP クライアントが `session/new` で渡してくる `mcpServers` は、今はすべて warn を出して無視している
（`serve/acp.rs`）。渡されるのは主に stdio のコマンドで、それを起動するのは agent 側になる。
agent はクライアントとは別のホストで動いているので、起動しても意味がないためだ。

MCP over ACP（`"type": "acp"`）なら、MCP のメッセージは既存の ACP 接続の上を流れ、ツールは
クライアントのプロセスの中で動く。agent がどこで動いていても、クライアント側のツールを使える。

一番の利用者は、作る予定のゲームだ。ゲームはエージェントとの会話に ACP を使い、
エージェントがゲームに働きかける手段として、MCP over ACP で自分のツールを出す。
Zed と acpx は 2026-11-10 の時点で MCP over ACP を実装していないが、それとは関係なく必要になる。

ACP v2 はクライアント側の fs / terminal を削り、代わりにクライアントが MCP サーバーを出す形を
推奨している。今回の作業はその方向にも合っている。ただし v2 への対応自体は #390 で行い、
この仕様は v1 のままで閉じる。

## 決めたこと

- **ACP は v1 のまま**。`unstable_protocol_v2` は有効にしない。
- **受け入れるのは `acp` トランスポートだけ**。クライアントが渡す `stdio` / `http` / `sse` は、
  今と同じく無視して warn を出す。
- **v1 のクライアント側 fs / terminal の振り分け（#270）は変えない**。
  `shell` / `file_*` / `dir_*` / skill 系の扱いは、今と同じままにする。
- **agent 側の MCP over ACP クライアントは、rmcp の `Transport` を自作して作る**。
  既存の `McpClient` を、トランスポートだけ差し替えて使い回す。手書きの小さなクライアントは作らない。
- **ツール名は `acp__<serverName>__<tool>`**。
- **権限は、MCP のツール annotations から `ToolKind` を決める**（§4）。
- **カスタムの ACP メッセージは使わない**。SDK 5.1 の `unstable_mcp_over_acp` が RFD どおりの
  `mcp/message` を v1 で持っているので、それを使う。

## 1. SDK を 5.1 に上げる

`agent-client-protocol` を `3.3.0` から `5.1` に上げ、`unstable_mcp_over_acp` を有効にする。

5.0 は破壊的変更を多く含む。主なものは次のとおり。

- `v1` / `v2` へのモジュール分割と、`v1::schema` / `v2::schema` を経由した型の参照
- `Builder::new` が特定のバージョンに依存しない形になった
- SDK の `Error` がバージョンに依存しない形になった
- request handle の ID がバージョンごとの型になった
- JSON-RPC の batch 対応が削除された

`serve/acp.rs` をこれに追従させる。この段階では挙動を変えず、既存の ACP テストが通ることを
完了の条件にする。

この段階は単独のコミットにする（scope は `deps`）。追従だけで既存のテストが通ることを
先に確かめてから、MCP over ACP の作業に入るためだ。

## 2. ACP トランスポート（`AcpMcpTransport`）

`mcp_client/` に、rmcp の `Transport<RoleClient>` を実装した `AcpMcpTransport` を足す。

- **送信**
  - rmcp が出す JSON-RPC の request は、ACP の `mcp/message` request（agent からクライアントへ）に包む。
    `params` は `serverId`、新しく発番する `requestId`、MCP の `method`、MCP の `params` から作る。
  - 応答の `result.result` は、MCP の結果として rmcp に返す。
  - 応答の `result.error`（`-33001` サーバーに届かない、`-33002` バックエンドの失敗など。
    外側の ACP エラーではなく、MCP over ACP の取り決めとしてのエラーコード）は、MCP の JSON-RPC
    エラーとして rmcp に返す。
  - 外側の ACP エラー（method not found など）も、MCP のエラーとして返す。
- **受信**: クライアントから来る `mcp/message` の notification（id がないもの）は、`requestId`
  で呼び出し中の request を探し、その request に紐づく通知として rmcp に渡す。
  該当する request がなければ捨てて、debug ログだけ出す。
- **取り消し**: rmcp の `notifications/cancelled` は、該当する外側の ACP request への
  `$/cancel_request` に変換する。
- **切断**: ACP 接続が閉じたら、保留中の request をすべてエラーで終わらせ、トランスポートを
  閉じた状態にする。`McpClient` の再接続処理は、閉じた ACP トランスポートを作り直さない。
  セッションが生きていても、接続がなければ使えないためだ。
- **バージョン**: RFD は MCP 2026-07-28 だけを対象にしている。rmcp 側の lifecycle は
  `ClientLifecycleMode` で 2026-07-28 に固定し、`initialize` へのフォールバックはしない。

`McpClient` は、今の `McpServerConfig` から作る経路に加えて、トランスポートを直接受け取る
コンストラクタを持つようにする。これでトランスポートを差し替えられる。
再接続、`tools_changed`、MRTR、elicitation への応答、`tool_result_text` は、既存のまま使う。

**リスク**: rmcp の `Transport` は「メッセージの流れ」として作られている。`mcp/message` の
「1 つの request を 1 つの封筒に包む」形とうまく合うかは、まだ確かめていない。
実装計画の最初のタスクでこれを確かめる。合わなかった場合は、`tools/list` と `tools/call` だけを
話す手書きのクライアントに切り替える。その場合は、この仕様を改めてから進める。

## 3. セッションでの扱い

- **能力の宣言**: `initialize` の応答の `agentCapabilities.mcpCapabilities` に `acp: true` を足す。
- **`session/new` と `session/load`**
  - `mcpServers` のうち `acp` 型のものそれぞれについて、`AcpMcpTransport` を持つ `McpClient` を作り、
    `tools/list` を呼ぶ。
  - 取れたツールは、そのセッションの `SessionToolSet` に登録する。
  - `tools/list` の取得は `session/new` の応答を待たせない。セッションを返したあと非同期に取り、
    最初のターンの前に結果を待つ。待つ時間には上限を付け、超えたらツールなしで始める。
  - `stdio` / `http` / `sse` のものは warn を出して無視する。warn には、MCP over ACP しか
    受け付けないことと、agent 自身の MCP は `[[tools.mcp_servers]]` で設定することを書く。
- **ツール名**: `acp__<serverName>__<tool>`。
  - `serverName` と `tool` は、ツール名に使える文字（英数字、`_`、`-`）に整える。
  - 1 つのセッションの中で `serverName` が重複したら、後のものに `_2`、`_3`… と番号を付け、warn を出す。
  - 全体の長さがプロバイダの上限を超える場合は、既存の `mcp__…` ツールと同じやり方で切り詰める。
- **寿命**
  - `SessionToolSet` と、その中の `McpClient` はセッションに属する。
  - ACP 接続が切れたとき、またはセッションが閉じられたときに捨てる。永続化はしない。
  - `session/load` で同じセッションが再接続されたら、クライアントがその時に渡してくる
    `mcpServers` で作り直す。
- **ツール一覧の更新**: `tools/list_changed` を受けたら `tools_changed` を立て、次のターンの前に
  `tools/list` を取り直す。既存の MCP と同じ仕組みだ。

## 4. ターンへの組み込みと権限

**ターンへの組み込み**

- 共有の `ToolSet`（全接続で一つ）には足さない。
- `run_llm_turn` は `TurnHost` 経由でセッションの `SessionToolSet` を受け取る。
  ACP 以外のセッションと、MCP over ACP のサーバーがないセッションでは空になる。
- **モデルに見せるツール一覧**: 共有のツール一覧の後ろに、セッションのツールを足す。
  `visible_tool_predicate` は共有のツールにだけかける。セッションのツールの表示は、
  クライアントが出したかどうかだけで決まる。
- **実行**: `acp__` で始まる名前はセッションのツールで、それ以外は今と同じく共有の `ToolSet` で実行する。
  共有の `ToolSet` は `acp__` で始まる名前を登録しない。サーバー側の MCP は `mcp__` なので、
  名前は衝突しない。

**権限**

`policy::decide` に渡す `ToolKind` は、ツールの annotations で決める。

| annotations | `ToolKind` | ACP `default` | ACP `accept_edits` | ACP `bypass` |
|---|---|---|---|---|
| `readOnlyHint: true` | `Read` | 確認しない | 確認しない | 確認しない |
| `readOnlyHint` が true でなく、`destructiveHint: false` | `Edit` | 確認する | 確認しない | 確認しない |
| どちらもない、または `destructiveHint: true` | `Other` | 確認する | 確認する | 確認しない |

annotations が付いていなければ、安全側の `Other` になる。ゲームのツールは、annotations を付ければ
確認なしで動かせる。annotations はクライアントの自己申告だが、そのクライアント自身に確認を
出すかどうかを決めるだけなので、信頼してよい。

## 5. エラー処理

| 状況 | 挙動 |
|---|---|
| `tools/list` が失敗した、またはクライアントが `mcp/message` を実装していない | セッションは成り立たせ、そのサーバーのツールは登録しない。warn ログを出す（method not found のときは「クライアントが MCP over ACP を実装していない」と書く）。`list_changed` を受けるか、次の `session/load` で再試行する |
| ツール呼び出しが MCP 側でエラーになった（`-33001` / `-33002`、MCP の `isError` など） | モデルにはツールのエラーとして文字列で返す。ターンは止めない |
| 呼び出しの途中で ACP 接続が切れた | 保留中の呼び出しをすべてエラーで終わらせ、`McpClient` を捨てる |
| 応答がない | 既存の MCP 呼び出しと同じ既定のタイムアウトを使う。時間切れなら `$/cancel_request` を送り、モデルにはタイムアウトを返す |
| `serverName` が重複した、または使えない文字を含む | §3 のとおり整え、番号を付けて warn を出す |

## 6. テスト

- **`AcpMcpTransport` の単体テスト**: 偽のピアを使って、次を確かめる。
  - request が `mcp/message` に包まれること（`serverId` と `requestId`）
  - 結果、carrier のエラー、外側の ACP エラーがそれぞれ正しく戻ること
  - notification が `requestId` で振り分けられること（該当しないものは捨てられること）
  - 取り消しが `$/cancel_request` になること
  - 切断したとき、保留中の呼び出しがすべてエラーで終わること
- **annotations から `ToolKind` への対応**: §4 の表をそのまま表形式のテストにする。
- **ツール名の整形**: 使えない文字、重複時の番号付け、長さの上限。
- **往復テスト**: SDK 5.1 の `with_mcp_server` で、テストの中に ACP クライアント（MCP の提供側）を
  立てる。`/acp` 経由で agent につなぎ、次を確かめる。
  - `session/new` の後、`acp__…` がツール一覧に出ること
  - ターン中の `tools/call` が成功し、結果がモデルに届くこと
  - `list_changed` でツール一覧が取り直されること
  - `stdio` / `http` で渡されたものは無視されること
  - 別のセッションにはツールが見えないこと

  このテストは、ゲーム側でツールを出す実装の見本も兼ねる。
- **既存の ACP テスト**: §1 の後でも、すべて通ること。

## 7. ドキュメント

- `docs/sapphire-agent-tools.md` に、クライアントが出すツール（`acp__…`）、受け付けるのは
  MCP over ACP だけであること、annotations による権限の扱いを書く。
- `config.example.toml` の MCP の説明に、クライアントが出す MCP は設定不要で、
  MCP over ACP で受け付けることを一行足す。
- `serve/acp.rs` の、`mcp_servers` を無視している箇所のコメントと warn を書き直す。
- コミットの scope は、SDK の更新が `deps`（`cliff.toml` で changelog から外れる）、
  それ以外が `acp` または `mcp`（agent の内部の scope なので changelog に載る）。
  `cliff.toml` は変えない。

## 8. この仕様でやらないこと

- ACP v2 への対応。v2 セッションでの shell / fs の扱い（サーバー側への統一、または `client_*` で
  名前を分ける案）、`host_access` の見直し、skill 系の置き場所。いずれも #390。
- クライアントが渡す `stdio` / `http` の MCP サーバーの受け入れ。
- agent がクライアントに出す MCP サーバー（向きが逆のもの）。
- ゲーム本体。
