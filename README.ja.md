# sapphire-agent（日本語）

> 言語: [English](README.md) | **日本語**

ファイルベース・ローカルファーストな`sapphire-framework`ワークスペースで、あなたと共に働くパーソナルAIアシスタントエージェント。MatrixとDiscordを通じて会話します。

> **ステータス：パーソナルプロジェクト。** これは私自身の利用のために作ったものです。私の環境で動くことだけが必要であり、私がテストするのもその環境だけです。使っていただいても、フォークしていただいても、プルリクエストを送っていただいても構いませんが、私自身が使わないプロバイダ・チャネル・機能のメンテナンスはしません。ユースケースが私と重なればラッキー、そうでなければ自由にフォークしてください。
>
> もともとこれが存在する理由は、試した他のエージェント（openclaw、zeroclawなど）が、必要としていた機能をサポートしていなかったり、私が気にする部分を実際にテストしていなかったり、修正を受け入れてくれなかったりしたからです。それで自分で書きました。期待値はそのあたりで調整してください。

## できること

- **チャネル**： Matrix（`matrix-sdk`によるE2EE）とDiscord（`serenity`）を並行稼働。
- **プロバイダ**： SSEストリーミングとマルチラウンドのツール使用ループを備えたAnthropic Messages APIに加え、OpenAI互換バックエンド（ローカルLLM、OpenRouterなど）を `[providers]` / `[profiles]` / `[room_profile]` スキーマでルーム／セッションごとに選択可能。
- **ワークスペース**： [`sapphire-framework`](https://github.com/sapphire-turtle/sapphire-framework) をバックエンドに使用 — ファイルインデックス、全文＋ベクトル検索（redb＋tantivy、デフォルトでLanceDBベクトル付き）。
- **シェル・ファイルツール**： `file_read`、`file_write`、`file_append`、`file_delete`、`dir_list`、`dir_walk`、`shell`、それに長時間実行用の `shell_start` / `shell_output` / `shell_kill`。これらの**届くマシンは名前ではなくセッションで決まります**。ACPセッション内では、接続したエディタが動いているマシンに、`/acp`の`fs/*`と`terminal/*`リクエストを介して作用します。ただし接続したエディタが`initialize`で宣言した機能に対してのみで、欠けた機能はエージェント自身のディスクへのフォールバックなしに非表示になります。それ以外（Matrix、Discord、`/rpc`、音声、`/a2a`、ハートビート、autonomous、サブエージェント）ではエージェント自身のマシンに作用し、オプトインです： `[tools.host_access] enabled = false`がデフォルトで、すべてのオージンに適用されます。有効化は意図的な行為で、有効化したうえでエージェントをコンテナで動かすのが推奨されます。下記「シェル・ファイルツール：誰のマシンか」参照。
- **セッション**： 人間が読める[`grain-id`](https://crates.io/crates/grain-id)エイリアス、自動生成タイトル、再開時の履歴ダンプ。
- **バックグラウンド**： ハートビートcronタスク、定期的なメモリー圧縮、定期的なワークスペース再インデックス、キャッチアップ付きの日次／週次／月次／年次ログ。
- **音声**： サーバー側の STT → ターン → TTS パイプライン（`/rpc` の `voice/*`）。ローカル STT/TTS は `sherpa-onnx` 経由。`sapphire-agent-desktop` などの音声クライアント向け。`sapphire-agent-cli voice` サテライトは CLI の本バイナリへの統合時に削除された（[#278](https://github.com/sapphire-turtle/sapphire-agent/issues/278)）。この役割はウェアラブルデバイスのファームウェアへ移行予定。
- **アンビエント音声インジェスト**： オプションの常時キャプチャ（ウェアラブル／ペンダントデバイスから）。`POST /audio/ingest`がベアラー認証済みデバイスから生の音声（メタデータはクエリパラメータ。JSON/base64フレーミングなし）を受け取り、再ゲーティングし、文字起こしし、ワークスペースで精選された参照音声Against話者を特定し、文字起こしをワークスペース外に保存します。「答えずに記録する」：このパスでは何もLLMターンを開始しません。`transcript_read`、`speaker_candidates`、`speaker_promote`が結果をエージェントツールとして公開します。デフォルトは無効 — `[ambient].enabled = true`で有効化。`config.example.toml`参照。
- **エージェント間**： `/a2a`エンドポイントはデバイス別のベアラートークン認証付きでv1 A2Aプロトコル（JSON-RPC `SendMessage`、AgentCard）を話します — `[a2a].enabled = true`で有効化。
- **サブエージェント**： メインエージェントが`subagent`ツールでタスクを委譲できる`<workspace>/agents/<name>.md`定義 — 独自のシステムプロンプト、独自のツールループ、最終回答のみが返り、ハンドルで後のラウンドに再開可能。定義のフロントマターの`profile:`は省略可能。書くとその`[profiles.<name>]`のプロバイダ（`fallback_provider`込み）で親の代わりに走り、省略時は委譲元のターンと同じモデルで走る — 後者が仕様変更前の既定動作そのままの、どちらも正当な書き方。configの`[profiles]`が定義していない名前なら起動時に即失敗する。プロバイダは親から引き継がれないことがある唯一の要素であり、権限ゲート・ツール一覧・隔離はプロファイルの有無で一切変わらない。下記「サブエージェント」参照。
- **サブエージェントのネスト**： 委譲は再帰でき、段数の上限はconfigの`[tools.subagent] max_depth`で決まる（既定2。メインエージェントが段数0 → 委譲先が1 → その先が2）。`1`は従来の「常に1段のみ」動作、`0`は委譲そのものを禁止し、メインエージェントのターンからも`subagent`ツールが見えない。定義のフロントマターの`subagents:`は`tools:`と同型の許可リストで、省略時は委譲元ターンに見えているエージェント集合をそのまま継承、`[]`は段数と無関係にそれ以上委譲しない、リストはその名前に限定する。ネストしたサブエージェントもこの段数上限と、各段の許可リストの連鎖の両方に従い、各ネスト段は個別に`[tools.subagent] turn_timeout_secs`の締切対象になる。
- **スキル**： エディタの*マシン*上にあるチェックアウトからACP経由でリクエスト時に読み込まれる、手順の書き下げ（プランニング、TDD、デバッグ、コードレビューなど）。ターミナルサポートを宣言したACPクライアントが必要 — Matrix、Discord、音声では完全に無効。下記「スキル」参照。
- **エディタ統合**： Zed（などのACPクライアント）が稼働中のエージェントをAgent Client Protocolで操作できる。エージェントが動いているマシンでは `sapphire-agent acp` を起動させる（stdio。ローカルのIPCで稼働中のエージェントへ中継し、`[acp].stdio_room_profile` が必要）。別のマシンからはWebSocketの`/acp`エンドポイントを使う（`[acp].enabled = true`で有効化）。下記「Zed / ACP」参照。
- **コマンド**：
  - `sapphire-agent` — チャネルリスナー＋JSON-RPC HTTP制御API（`/rpc`、`/a2a`、`/acp`）を開始
  - `sapphire-agent init [PATH]` — エージェントが読むファイル（`AGENTS.md`、`SOUL.md`など）でワークスペースをシードし、貼り付けるホストローカル設定を出力。既存ファイルを決して上書きしない
  - `sapphire-agent verify` — 設定を検証し、device -> room_profileのバインディングを含む読み込まれたワークスペースファイルを報告
  - sapphire-framework の動詞（稼働中のサーバーまたは bridge と話す）: `status`、`service install|uninstall|status`、`workspace init|select|show|…`（エージェントは選択されたワークスペースを扱う。`config.toml` の `workspace_dir` は初回起動時にだけ取り込まれ、切り替えるとエージェントは新しいワークスペースで再起動する）、`workgroup`、`device`、`embedding`、`external-device add|list|rotate|retire|restore|apps`（ベアラートークンでエージェントにアクセスするクライアント。`add` と `rotate` はトークンを一度だけ表示）
  - `sapphire-agent chat` — 同じホストで動いているエージェントとローカル IPC 経由で対話する REPL（`-m TEXT` で単発、`--list`、`--history`、`--session ID`、`--json`）。`[cli].room_profile` のルームプロファイルで動く

## インストール

```sh
curl -fsSL https://raw.githubusercontent.com/sapphire-turtle/sapphire-framework/main/install/install.sh | sh -s -- agent
```

```powershell
# Windows
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/sapphire-turtle/sapphire-framework/main/install/install.ps1))) agent
```

`sapphire-agent` と、同期に使う `sapphire-bridge` が一緒に `~/.local/bin` へ入ります。デスクトップクライアントも入れるには `agent-desktop` を足してください。オプションは[インストールスクリプトの README](https://github.com/sapphire-turtle/sapphire-framework/tree/main/install)を参照してください。

cargo で入れる場合：

```sh
cargo install sapphire-agent
```

またはソースから：

```sh
git clone https://github.com/sapphire-turtle/sapphire-agent
cd sapphire-agent
cargo build --release
```

リリースバイナリは**Linux**（x86_64、aarch64）、**macOS**（aarch64）、**Windows**（x86_64）向けに公開されています。それぞれ単一の自己完結ファイル — `sherpa-onnx`とそのONNX Runtimeは静的リンクされているため、一緒にインストールするものは何もありません。

Windows では `shell` ツールは、呼び出しで別のシェル（`pwsh`、`cmd`、`bash` など）を指定しない限り Windows PowerShell でコマンドを実行します。それ以外の OS では `$SHELL`、未設定なら `/bin/sh` を使います。

サーバーとその CLI は 1 つのバイナリです（[#278](https://github.com/sapphire-turtle/sapphire-agent/issues/278)）。`sapphire-ledger` など他の sapphire-framework アプリと同じく、クライアント系のコマンド（`chat`、`acp`、`status` など）は同じホストで動いているサーバーにローカル IPC で問い合わせます。GUI チャットクライアントの `sapphire-agent-desktop` も同じで、`chat` と同じく `[cli].room_profile` のルームプロファイルで動きます。

## 設定

空のワークスペースから開始します：

```sh
sapphire-agent init ~/sapphire-workspace
```

これはエージェントが読むファイル — `AGENTS.md`、`SOUL.md`、`IDENTITY.md`、`USER.md`、`TOOLS.md`、`BOOTSTRAP.md`、`memory/default/`ツリー、ハートビートタスクの例、そしてワークスペースレイヤーが設定を許可する設定を列挙した`.sapphire-agent/config.toml` — をシードします。ほとんどは意図的に空です：`BOOTSTRAP.md`は最初の起動時の儀式で、エージェントに呼び名を尋ねさせ、その回答を`IDENTITY.md`と`SOUL.md`に書き込ませ、その後自分自身を削除します。

`init`は決して上書きしないので再実行は安全です — それこそが古いビルドが作っておいたワークスペースがそれ以降に追加されたファイルを取り込む方法です。

ワークスペースの外には何も書き込みません。認証情報とマシンのパスはホストローカルなので、`init`はそのファイルを書き込まず、`~/.config/sapphire-agent/config.toml`（Linuxの場合）に置いてもらえるよう出力します。他に何を書けるかは`config.example.toml`を — チャネル、バインドアドレス、MCPサーバー、STT/TTSモデルパス。

それから：

```sh
sapphire-agent verify   # 設定とワークスペースのサニティチェック
sapphire-agent          # チャネルリスナー＋HTTP制御APIを開始
sapphire-agent chat     # そのエージェントとの対話セッション（[cli].room_profile が必要）
```

### 誰にも読まれないキー

どのフィールドも消費しないキーは、エラーではなく警告です。タイプ（`skils = true`）や間違ったテーブル下の設定（`skills`は`[memory_namespace.<name>]`のものです。`[room_profile.<name>]`の下ではパースは通るが何も起きません）はそのままでは静かに見過ぎさえます — serdeがそのキーを落とすため、書いた人には設定済み、エージェントには未設定として見えます。起動時にファイルごとに1件の警告をログし、`verify`と同じプロベナンスでファイル名を明示します：

```
WARN Ignoring 1 unrecognised key(s) in /home/you/.config/sapphire-agent/config.toml:
     room_profile.default.skills. Nothing reads these, so the agent is running as if
     they were absent — check for a typo, or for a setting written under the wrong table.
```

`verify`は同じ一覧を`Unknown keys`の下に出力します。これは意図的に`#[serde(deny_unknown_fields)]`ではありません：ハードエラーにすると、新しいビルド向けに書かれた設定が古いビルドでロードできなくなります。これは特にホスト間で移動するワークスペースレイヤーで重要です。

## 詳細ドキュメント

サブエージェント、Zed / ACP統合、スキル、既知の制限事項など、アーキテクチャの詳細な解説は英語版README（[README.md](README.md)）の該当セクションを参照してください。

- Subagents — サブエージェントの定義・委譲・ネスト・再開
- Zed / ACP — ACPエンドポイント、権限とモード、シェル・ファイルツール、過去セッションの読み込み
- Skills — スキルの読み込み元ディレクトリ、有効化、4つのツール
- エージェント自身の定義編集 — heartbeat / autonomous / agents の定義ファイルを読み書きする4ツールと `[tools.admin].room_profiles` の room_profile 許可リスト（ACP セッションからも使えます）

## ライセンス

このリポジトリは異なるライセンスのコンポーネントを含みます：

| コンポーネント | ライセンス |
|-----------|---------|
| `sapphire-agent`（`cli/`） | MIT OR Apache-2.0 |
| `sapphire-agent-core` | MIT OR Apache-2.0 |
| `sapphire-agent-desktop` | GPL-3.0-or-later、[App Storeマーケットプレイス例外](desktop/LICENSE.App-Store-Exception)付き |

各コンポーネントのディレクトリにある`LICENSE`（または`LICENSE-MIT` / `LICENSE-APACHE`）ファイルに完全なライセンス文書があります。デスクトップアプリは SIL Open Font License のフォント（[desktop/assets/LICENSE-OFL.txt](desktop/assets/LICENSE-OFL.txt)）と ISC License の Lucide アイコン（[desktop/assets/icons/LICENSE-LUCIDE.txt](desktop/assets/icons/LICENSE-LUCIDE.txt)）も同梱しています。
