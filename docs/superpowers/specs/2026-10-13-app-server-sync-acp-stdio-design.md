# アプリサーバー化・同期・stdio ACP

- **対象**: `server/src/main.rs`、`server/src/serve/acp.rs`、`server/src/device_auth.rs`、
  `server/src/channel/`、`server/src/heartbeat.rs`、`server/src/autonomous.rs`、
  `server/src/periodic_log.rs`、`server/src/memory_compaction.rs`、`server/src/config.rs`、
  `crates/sapphire-agent-core/`
- **前提**: #285（external device 認証）、#286（MCP サーバー削除）、#287（イベント単位のセッションファイル）
- **参照（framework）**: `2026-09-16-process-architecture-design.md`、
  `2026-10-11-single-workspace-server-design.md`（#215）、`2026-10-08-primary-device-design.md`（#182）、
  `docs/ARCHITECTURE.md` の MCP 節。移行の型は `sapphire-journal` の `cli/src/serve.rs`。
- **状態**: 提案

## なぜ

今のエージェントは 1 台のホストで動き、エディタ（Zed）は HTTP の `/acp` に遠隔からつなぐ。
エージェントループ・LLM 呼び出し・ツール実行はすべてそのホストで起きる。

これを、**各ホストがワークスペースの同期済みコピーを持ち、エディタは手元のホストのエージェントに
stdio でつなぐ**形にする。エージェントループはクライアント側（エディタのあるホスト）で直接走り、
従来の HTTP `/acp` による遠隔アクセスも残す。

そのためには、エージェントを framework の作法どおり**アプリサーバー（`AppServer`）**として動かす
必要がある。同期（`SyncRuntime`）も、stdio のサブコマンドがサーバーへ届く経路（IPC エンドポイント）も、
`AppServer` が提供するものだからである。framework の `ARCHITECTURE.md` はすでに
「将来の stdio ACP」を前提に書かれている（stdout をプロトコルに使うサブコマンドのため、
コンソールログは stderr に出す — #232）。

## 決定事項

### 1. `sapphire-agent-server` を `AppServer` に載せる

`sapphire-journal` と同じ形にする。

- 裸の起動と `serve` は `AppServer::run`。framework が IPC ソケット、現在のワークスペース、
  シグナル、ログファイルを持つ。
- framework のワンショット動詞は `FrameworkCommand` でそのまま提供する:
  `status` / `service` / `workspace` / `workgroup` / `device` / `external-device` / `embedding`。
  #285 で自前実装した `external-device` は `FrameworkCommand::ExternalDevice` に置き換える
  （`add` の既定アプリは `AppServer` のアプリ名から入る）。
- エージェント独自の IPC メソッドは `AppServer::extend` で `sapphire-agent.*` として足す（6 で使う）。
  `app_api` の版は `crates/sapphire-agent-core` に定数として置き、stdio 側と共有する。
- `init` / `verify` はエージェント独自のサブコマンドとして残す。

### 2. ワークスペースは framework の選択に従う

- 対象のワークスペースは `config.toml` の `workspace_dir` ではなく、framework の選択
  （`<config dir>/workspace.toml`、`workspace select` / `init`）で決まる。
- **移行**: 選択がなく `workspace_dir` が設定されていれば、起動時にそれを `workspace.select` する。
  以降 `workspace_dir` は警告付きで無視する（`[device.*]` と同じ migration エラーにはしない —
  移行は自動でできるため）。
- `WorkspaceState` は自前で開かず、`server.host().backend().await?.state()` の `Arc<WorkspaceState>` を使う。
  redb のキャッシュは 1 プロセスしか開けないため、自前で開くと framework と衝突する。
  `WorkspaceState` に `&mut self` のメソッドはないので、現在の `Arc<Mutex<WorkspaceState>>` は
  `Arc<WorkspaceState>` に置き換えられる（呼び出し側 103 箇所は機械的な変更）。
- **ワークスペースの切り替え**（`workspace select`）は、当面**プロセスの再起動**で扱う。
  エージェントはセッションストア、メモリ、ブートストラップファイル、ツールを丸ごとワークスペースの
  ルートに結び付けているので、生きたまま差し替えると状態が半端に残る。`workspace.event` で
  切り替えを検知したら正常終了し、サービスマネージャに起こし直させる（`ManagedBy::Service`
  でない起動では、終了せずに警告だけ出す）。生きたままの切り替えは後続の課題とする。

### 3. HTTP は当面エージェント自前のリスナーのまま

`AppServer::http` に移すと、framework の `protect` が**すべての**ルートに Bearer を要求する。
しかしエージェントには、認証なしで公開すべきルート（A2A の `/.well-known/agent-card.json`）と、
JSON-RPC のエラー形式で認証失敗を返したいルート（`/rpc`、`/a2a`）がある。そのため ④ では移さず、
自前の axum リスナーと #285 の `DeviceAuth` を使い続ける。framework 側に「保護しないルート」の
口ができたら移す。

### 4. 同期

- `BridgeClient::connect` が通れば `SyncRuntime` を付け、通らなければ同期なしで起動する
  （journal と同じ。bridge がないことは致命的でない）。
- 同期されるのはワークスペース全体。セッションは #287 でイベント単位のファイルになったので、
  2 台が同じセッションに書いても同じファイルには書かない。
- **同期されないもの**: ツールペイロードのキャッシュと画像キャッシュは、索引に入れないために
  ワークスペースの外（`<cache_dir>`）に置いている。そのため、別のホストで書かれたセッションを
  読むと、ツール結果は `MISSING_RESULT` のプレースホルダーになる（対の構造は保たれる）。
  ④ ではこれを受け入れ、未決事項とする。

### 5. 1 台だけで走らせる仕事は代表デバイスに限る

全ホストがサーバーを動かすと、次の仕事が全ホストで走ってしまう:

| 仕事 | 重複すると |
|---|---|
| Matrix / Discord チャネル | 同じメッセージに複数のホストが返信する |
| heartbeat | 通知が重複する |
| autonomous セッション | 同じタスクを複数のホストが実行する |
| 日誌・週報などの periodic log | 同じ日のログを複数のホストが書く |
| memory compaction | 同じファイルを並行して書き換える |

これらは `SyncRuntime::is_primary(workspace_id)` が真のホストだけで走らせる。framework は代表デバイスを
まさに「1 台が workgroup の代わりにやる背景作業」のために選出している（#182）。

- 同期が無効、または bridge がないときは、このホストが唯一の書き手なので常に走らせる。
- ループ型の仕事（heartbeat、autonomous、periodic log、compaction）は、各回の実行前に問い合わせる。
- チャネルは常駐接続なので、30 秒ごとに役割を見て、代表になったら接続し、外れたら切る。
  代表の交代は数十秒単位で起きうるので、交代の瞬間の 1 通は取りこぼすか重複しうる。
  これは受け入れる。
- リクエスト駆動のもの（`/rpc`、`/a2a`、`/acp`、ambient ingest、stdio ACP）は全ホストで受け付ける。

### 6. stdio ACP: `sapphire-agent acp`

エディタが子プロセスとして起動するサブコマンド。framework の `mcp` サブコマンドと同じく、
**手元で動いているサーバーに IPC でつなぎ、サーバーを起動はしない**。エージェントループは
そのサーバー（同じホスト）で走る。

```
エディタ ──stdio──▶ sapphire-agent acp ──IPC──▶ sapphire-agent serve（同じホスト）
                     （中継のみ）                 ACP の接続処理・エージェントループ
```

**中継は ACP の行を IPC で運ぶトンネルにする。** ACP は双方向で、エージェントからエディタへの
リクエスト（権限確認、ファイル読み込み、端末）もある。メソッドごとに翻訳すると ACP の版が
変わるたびに中継側も直すことになるので、中継は JSON-RPC の 1 メッセージ（1 行）を中身に
触れずに運ぶだけにする。

- `sapphire-agent.acp.open` → `{ conn }`: サーバー側で ACP 接続を 1 本作る。
- `sapphire-agent.acp.send { conn, line }`: エディタ → エージェントの 1 行。
- 通知 `sapphire-agent.acp.recv { conn, line }`: エージェント → エディタの 1 行
  （`PeerHandle::notify`）。
- `sapphire-agent.acp.close { conn }`、または IPC 接続が切れたら、その ACP 接続を閉じる。

サーバー側では、今 WebSocket に結び付いている `serve_connection` を「1 行ずつ読み書きできる
トランスポート」に一般化し、WebSocket と IPC トンネルの両方から使う。ACP の処理は 1 つのまま。

**認証とルーム プロファイル**: IPC は同じ OS ユーザーにしか開かない（framework の peer uid 確認）
ので、Bearer トークンは使わない。トークンがないためルーム プロファイルが決まらないので、
`[acp].stdio_room_profile` で指定する（未設定なら stdio ACP は起動時にエラーで、その旨を言う）。

**却下した案**: エージェントが ACP 専用の生ソケット（`connect_raw`）を別に開く案。
バイトを素通しできて簡単だが、エンドポイントが 2 つになり、framework のハンドシェイク
（版の確認）を通らない。

### 7. 遠隔の HTTP `/acp` は残す

同期していないマシンからエディタを使う場合のために、HTTP の `/acp` は今のまま残す
（external device のトークンで認証）。stdio と HTTP は同じ ACP の処理に入る。

## PR の分け方

1. **④a アプリサーバー化**: 1・2・3。`FrameworkCommand`、ワークスペース選択と移行、
   `WorkspaceState` を host から取る、切り替え時の再起動。同期はまだ付けない。
2. **④b 同期**: 4・5。`SyncRuntime`、代表デバイスでの単一実行。
3. **④c stdio ACP**: 6・7。IPC トンネル、`serve_connection` の一般化、`acp` サブコマンド、
   `[acp].stdio_room_profile`。

各 PR は前の PR の上に積む。

## テスト

- ④a: 選択がないときに `workspace_dir` を選択へ移す、選択があれば `workspace_dir` を無視する、
  `FrameworkCommand` のパース、切り替えイベントで終了する（サービス管理下のときだけ）。
- ④b: `is_primary` を差し替えられる形にして、代表でないホストではチャネルとループが走らないこと、
  同期なしでは走ること、役割が変わったらチャネルが接続・切断されること。
- ④c: プロセス内の IPC 接続（`Connection::pair`）でトンネルを通し、`initialize` → `session/new` →
  `session/prompt` が往復すること。エージェントからエディタへのリクエスト（権限確認）がトンネルを
  逆向きに通ること。IPC が切れたら ACP 接続が閉じること。`stdio_room_profile` 未設定のエラー。

## 未決事項

1. **ツールペイロードと画像のキャッシュを同期するか。** しないなら、別ホストのセッションでは
   ツール結果が失われる。ワークスペース内の索引対象外の場所に置いて同期する、bridge の blob で
   運ぶ、などの案がある。
2. **セッションの分岐の統合。** 同じセッションを 2 台で同時に続けると分岐する（#287 では古い枝に
   従い、日誌には両方残す）。stdio ACP では、セッションはエディタのいるホストで続くので、
   分岐は同じセッションを 2 台で同時に開いたときに限られる。統合するかは様子を見て決める。
3. **生きたままのワークスペース切り替え**（2 では再起動で扱う）。
4. **HTTP を `AppServer` に移す時期**（3）。framework 側に保護しないルートの口が要る。
5. **ホスト固有の設定。** LLM の API キーなどは各ホストの `config.toml` に要る（ワークスペース層は
   資格情報を持てない）。新しいホストを足すたびに書く必要があることを、ドキュメントで案内する。
