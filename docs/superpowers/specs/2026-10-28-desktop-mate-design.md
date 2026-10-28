# デスクトップメイト（VRM キャラクター）と GUI ライブラリ化

- **対象**: `desktop/`、新設 `crates/sapphire-agent-gui/`、`crates/sapphire-agent-core/`、
  `cli/src/tools/`、`cli/src/serve/`（chat IPC と avatar IPC）、`cliff.toml`、`release-plz.toml`
- **参照（framework）**: `sapphire-framework-gui` 0.23 の `Settings`（`settings.rs`）、
  `mobile/`（サーバー内蔵の型）
- **外部**: bevy 0.20、bevy_egui（fork）、bevy_vrm1（fork）
- **状態**: 提案

## なぜ

`sapphire-agent-desktop` は bevy + egui のチャット画面だけで、bevy を選んだ理由だった
3D キャラクター（`desktop/Cargo.toml` のコメント参照）はまだない。これを、デスクトップの上に
常駐する透過ウィンドウの VRM キャラクター（デスクトップメイト）として実装する。

キャラクターの表情とジェスチャーは、本文を別モデルで感情解析するのではなく、**エージェントが
ツールを明示的に呼んで**決める。そのためにツールが取る値の規定（VRM 1.0 のプリセット）を持つ。

他の sapphire アプリと同じくモバイル版も作る。モバイルは framework の型どおり**サーバーを
同じプロセスに内蔵**する。今回はデスクトップで形にすることを最優先しつつ、モバイルと共有する
ほぼすべてをライブラリクレートに置き、プラットフォーム固有の部分を薄い bin に閉じ込める。

## 決定事項

### 1. framework の `Shell` は使わない。部品（`Settings`）だけ使う

キャラクターが主役で、モバイルでは全画面に出すため、レール・右ペインを持つ `Shell` は
採らない。framework-gui からは `Settings` を単体で使う
（`Settings::new(client)`、毎フレーム `poll()` / `follow_theme(ctx)` / `ui(ui)`、
`health()` / `status_ui()`、`i18n::register(ctx, RESOURCES)`）。アプリ独自の設定は
`with_app_sections` で「キャラクター」ページとして足す。

framework-gui の eframe 用起動（`app::run`）は使えないので、同等の処理を bevy 側で持つ:
tokio ランタイム、`ClientConfig::new` → `FrameworkClient::spawn`、フォント・テーマ、
repaint コールバックで bevy の winit を起こす処理。

### 2. 依存は fork して git 参照する

この GUI は crates.io に公開しない（`publish = false`）ので、バージョンが合わない依存は
fork して git 参照してよい。

| 依存 | 理由 | 変更 |
|---|---|---|
| bevy_egui | 最新 0.43.x は egui 0.36、framework-gui 0.22 以降は egui 0.37 | egui を 0.37 に上げる（上流 dependabot PR #508 相当）。上流が出たら戻す |
| bevy_vrm1 | VRM 1.0 の expressions / look-at / spring bone / MToon / VRMA を持つ唯一の候補。bevy 0.19 | bevy 0.20 に上げる |

`bevy_vrm`（unavi）0.5 は bevy 0.20 対応済みだが expressions と look-at が未対応なので採らない。
bevy_vrm1 は VRM 1.0 専用（0.x は読めない）。VRoid Studio の現行版は 1.0 を書き出せるので許容する。

### 3. クレート構成

```
crates/sapphire-agent-gui/   lib, publish = false（git 依存可）
  ├─ app        起動：runtime、FrameworkClient、フォント・テーマ、repaint
  ├─ chat/      チャット状態・bridge（今の desktop/src/bridge.rs, state.rs）と egui ビュー
  │             （入力欄、吹き出し、履歴パネル、メニュー）
  ├─ avatar/    プラットフォーム非依存の bevy plugin
  │   ├─ model       VRM 読み込み（bevy_vrm1）、未設定・失敗時の仮表示
  │   ├─ expression  目標値と遷移、時間経過で neutral へ戻す、まばたき
  │   ├─ action      VRMA の再生、idle のループ
  │   ├─ look        視線の目標（デスクトップ：カーソル、モバイル：正面）
  │   └─ assets      avatar IPC からの取得・アップロード・キャッシュ
  ├─ input      キャラクター上のジェスチャー → 入力欄 / 履歴 / メニュー
  ├─ settings   framework Settings の組み込み、「キャラクター」ページ
  └─ AgentGuiPlugin  上をまとめる。出し先（Presenter）は呼び出し側が渡す
desktop/                     bin：AgentGuiPlugin + 透過メイトウィンドウの presenter
mobile/                      （将来）bin：AgentGuiPlugin + 全画面 presenter + 内蔵サーバー
```

- `avatar` と `chat` には `cfg(target_os)` を入れない。デスクトップ固有のもの
  （透過・最前面・クリック透過・ドラッグ移動）は `desktop/` の presenter だけが持つ。
- ライブラリには今回モバイルのコードを書かない。境界だけ作る。
- 純粋なロジック（表情の遷移、まばたきのタイマー、ジェスチャー判定、manifest の差分計算）は
  bevy に依存しない関数・型にして単体テストできるようにする。

### 4. デスクトップのメイトウィンドウ

- 1 つの透過・枠なし・最前面ウィンドウ。タスクバーに出さない。3D カメラ＋その上に egui。
- ウィンドウはキャラクターより大きめに取り、吹き出し・入力欄・履歴・メニューはその中に
  キャラクターへ重ねて出す。
- クリックを受けるのは、キャラクターの当たり判定（バウンディングボックスを画面へ射影したもの）
  か egui が pointer を使っている所だけ。それ以外の所では毎フレーム `CursorOptions::hit_test`
  を切ってクリックを下のアプリへ通す。
- ウィンドウ位置は GUI のローカル設定に保存し、次回復元する。

### 5. 操作（キャラクター以外のボタンを持たない）

| 操作（デスクトップ / モバイル） | 結果 |
|---|---|
| クリック / タップ | 入力欄（キャラクターの近く） |
| キャラクター上でスクロール / 2 本指スワイプ | 履歴パネル（キャラクターに重なる半透明パネル） |
| 右クリック / ダブルタップ | メニュー（設定、終了など） |
| ドラッグ | キャラクターの移動（デスクトップのみ） |
| Esc | 開いている入力欄・履歴・メニューを閉じる |

- 返答は吹き出しで出し、文字数に応じた時間で消す。履歴にも残る。
- ツール呼び出し中は吹き出しに「考え中」を出す（`tool_start` / `tool_end` から）。
- アクセシビリティ: 設定「操作ボタンを表示」（GUI ローカル、既定オフ）で、入力・履歴・
  メニューのボタンをキャラクターの横に出す。UI は egui（AccessKit 対応）で作る。
  デスクトップではキーボードショートカットも持つ。App Store 審査の可否はモバイル版で確かめる。
- 将来（今回やらない）: パネルを開いたときキャラクターがよけて横から覗き込むモーション。

### 6. 表情・アクションのツール（cli / core）

#### ターンごとの申告（クライアントの能力）

`sapphire-agent.chat.send` に省略可能な `client` を足す。クライアントがこのターンで
何をできるかの申告で、クライアント依存のツールの出し分けはすべてこれ一つで行う。
今回解釈するのは `avatar` だけで、`speech` / `capture`（§12）は形だけ予約する。
サーバーは知らないキーを無視する。

```jsonc
{ "session_id": "...", "content": "...",
  "attachments": [{ "mime": "image/png", "data": "<base64>" }],   // 省略可（後述）
  "client": {
    "avatar": {
      "expressions": ["neutral", "happy", "angry", "sad", "relaxed", "surprised"],
      "actions": ["peace", "wave"]
    }
    // "speech": {...}, "capture": [...]  — §12、今回は未実装
  } }
```

- 申告を `open` ではなく `send` に載せるのは、同じセッションを desktop で開いた後で
  CLI の `chat` から再開しうるため。ターン単位ならサーバーは状態を持たない。
- `client.avatar` があるターンだけ LLM のツール一覧に `set_expression` を加え、
  `actions` が空でなければ `play_action` も加える。heartbeat / autonomous / Discord / Matrix の
  ターンには出ない。

#### 添付（今回実装する）

`attachments` はユーザーのメッセージに付ける画像。各要素は `{mime, data}`（base64）で、
`image/png` / `image/jpeg` / `image/webp` を受ける。サーバーは既存の `ContentPart::Image`
としてメッセージに載せ、`image_cache` の扱い（sha256 で外に逃がす）にそのまま乗せる。
1 件 20 MiB・1 ターン 8 件まで。超過や未対応の mime は `send` のエラーにする
（ターンは始めない）。GUI 側の添付 UI（ファイル選択・撮影）は今回作らない。
- 今回クライアントが申告する `expressions` は VRM 1.0 の感情プリセット 6 種のみ。
  モデル独自の custom expression への拡張はクライアントの申告だけで済む（サーバーは一覧から
  enum を組むだけ）。
- `blink*` / `aa` / `ih` / `ou` / `ee` / `oh` / `look*` はクライアントが自動制御し、申告しない。

#### `set_expression`

```
set_expression(expression: enum<申告された expressions>, intensity?: number 0.0–1.0 = 1.0)
```

- 説明文: 返答の気持ちに合わせて呼ぶ。1 ターンに何度呼んでもよい。
- 実行すると `progress` 通知を送り、`"ok"` を返す。
- 一覧にない名前・範囲外の intensity はツールエラー（有効な一覧を添える）。

#### `play_action`

```
play_action(action: enum<申告された actions>)
```

- 説明文: ジェスチャーを一度だけ再生する。挨拶・喜びなど、ここぞという場面でだけ使う。
- **1 ターン 1 回まで**をサーバーで強制する。2 回目は「このターンではもう使った」とエラーを返す。
- 実行すると `progress` 通知を送り、`"ok"` を返す。

#### 通知

```jsonc
// sapphire-agent.chat.progress（既存の tool_start / tool_end に加えて）
{ "session_id": "...", "kind": "expression", "expression": "happy", "intensity": 0.8 }
{ "session_id": "...", "kind": "action", "action": "peace" }
```

`set_expression` / `play_action` 自体の `tool_start` / `tool_end` も従来どおり流れる。
GUI はこの 2 つを「考え中」表示から除外する。

#### core の API

```rust
pub enum ChatEvent {
    Tool(ToolEvent),
    Expression { name: String, intensity: f32 },
    Action { name: String },
}

#[derive(Default)]
pub struct ClientCaps { pub avatar: Option<AvatarCaps> /* speech, capture は §12 で追加 */ }
pub struct AvatarCaps { pub expressions: Vec<String>, pub actions: Vec<String> }
pub struct Attachment { pub mime: String, pub data: Vec<u8> }

pub struct Turn<'a> {
    pub content: &'a str,
    pub attachments: &'a [Attachment],
    pub client: &'a ClientCaps,
}

impl ChatClient {
    pub async fn send(
        &self,
        session_id: &str,
        turn: Turn<'_>,
        on_event: impl FnMut(ChatEvent),
    ) -> Result<String>;
}
```

引数を `Turn` にまとめるのは、§12 の `speak` などを足しても `send` の呼び出し側を
壊さないため。`ChatEvent` は `#[non_exhaustive]` にする。CLI の `chat` は
`ClientCaps::default()` で呼び、`ChatEvent::Tool` だけ扱う。

### 7. モデルデータはサーバーが持ち、同期する

#### 置き場所

エージェントのワークスペースの `avatar/`。ワークスペースのファイルとして framework の同期に
乗る（同期の上限 1 GiB）。

```
<workspace>/avatar/
  avatar.toml      # model = "model.vrm"
                   # idle = "actions/idle.vrma"      （省略可）
                   # [actions]
                   # peace = "actions/peace.vrma"
  model.vrm
  actions/*.vrma
```

framework の indexer は markdown 系（`md` / `markdown` / `txt` / `rst` / `org`）と
画像拡張子しか索引しないので、`.vrm` / `.vrma` / `.toml` は索引に入らない。
除外の実装は不要。

#### IPC メソッド

| メソッド | 引数 → 結果 |
|---|---|
| `sapphire-agent.avatar.manifest` | `{}` → `{model?, idle?, actions: {name: path}, files: [{path, size, sha256}]}` |
| `sapphire-agent.avatar.read` | `{path, offset, len}`（len ≤ 1 MiB）→ `{data: base64, eof}` |
| `sapphire-agent.avatar.write_begin` | `{path, size, sha256}` → `{upload_id}` |
| `sapphire-agent.avatar.write_chunk` | `{upload_id, offset, data: base64}` → `{}` |
| `sapphire-agent.avatar.write_commit` | `{upload_id}` → `{}`（ハッシュ照合後に rename で置き換え） |
| `sapphire-agent.avatar.set_manifest` | `{model?, idle?, actions}` → `{}`（`avatar.toml` を書き直す） |
| `sapphire-agent.avatar.remove` | `{path}` → `{}` |

- `path` は `avatar/` からの相対で、`/` 区切り。`..`・絶対パス・`avatar.toml` 直接は拒否する。
- 1 ファイル 256 MiB まで。
- 書き込みは `avatar/` の外の一時領域に置き、commit で照合してから置き換える。
  未完成のファイルが同期に乗らないようにするため。放置されたアップロードは、サーバー側の `PeerHandle` に切断の通知が無いため、最後の書き込みから 10 分で捨てる（次の `write_begin` のときに掃除する）。
- `set_manifest` は、参照するファイルが存在しなければ拒否する。

#### GUI 側

- 接続時・自分のアップロード後・5 分ごとに `manifest` を取り、sha256 が手元のキャッシュ
  （GUI のキャッシュディレクトリ、sha256 名）に無いファイルだけ `read` で取る。
  他端末から同期で届いた変更も次の取得で反映される。変更通知は今回作らない。
- bevy の読み込みはキャッシュのファイルから行う。
- 「キャラクター」設定ページ: `.vrm` を選ぶとアップロードして `set_manifest`、
  アクション名と `.vrma` の組を追加・削除・改名、idle の指定。
- `send` で申告する `actions` は、読み込みに成功したものだけ。

#### モバイル

サーバーを内蔵するので、同じメソッドを同じプロセス内の IPC carrier 越しに呼ぶ。

### 8. キャラクターの振る舞い

- **表情**: 受けた目標へ約 0.2 秒で補間する。次の `set_expression` まで保持し、
  ターンの返答が終わってから **30 秒**（GUI ローカル設定）で `neutral` へゆっくり戻す。
- **まばたき**: 2〜6 秒のランダム間隔。表情の `overrideBlink` を尊重する。
- **視線**: デスクトップはカーソル、モバイル（将来）は正面。
- **揺れもの**: bevy_vrm1 の spring bone に任せる。
- **idle**: `idle` があればループ再生、なければ静止ポーズ。
- **アクション**: VRMA を一度再生して idle に戻る。再生中に来た次のアクションは無視する。
- **未設定・失敗**: 仮の表示（シルエット）。クリックで設定の「キャラクター」ページを開く。
  失敗の理由は吹き出しで出す。

### 9. GUI ローカル設定（同期しない）

表情を戻すまでの秒数、操作ボタンの表示、メイトウィンドウの位置。端末ごとの好みなので
サーバーには置かない。

### 10. バージョン

- `APP_API` を 2 → **3**。変更履歴: 「3: `chat.send` に `client`（`avatar`）と
  `attachments`、`chat.progress` に `expression` / `action`、`sapphire-agent.avatar.*` を追加」。
- ハンドシェイクが別の `APP_API` のサーバーを拒否するので、互換の分岐は持たない。
- §12 の機能はそれぞれ実装するときに `APP_API` を上げる。

### 11. リリース・changelog

- `sapphire-agent-gui` は `publish = false`。`release-plz.toml` で `release = false`
  （タグもリリースも作らない）。
- `cliff.toml` の除外スコープに `gui` を、最初の `(gui)` コミットと同時に足す。

### 12. 将来の拡張（形だけ決め、今回は実装しない）

音声入出力とカメラ・スクリーンショットは別の設計書・別の段階で実装する。ここでは、
後から足すときにプロトコルの骨格を変えずに済むよう、形だけを決める。名前・フィールドは
その設計書で詰めてよいが、次の方針は変えない。

#### 音声入力（STT、ウェイクワード）

- **ウェイクワードは GUI 側**で検出する（常時マイクを聞くため）。sherpa-onnx の
  keyword spotting を使い、`ort` は入れない（CLAUDE.md の ONNX Runtime 二重リンク問題）。
- 検出 → 発話区間の録音 → **サーバーで STT**（既存の `cli/src/voice` の `SttProvider`）→ 通常のターン。
- メソッド: `sapphire-agent.voice.transcribe {audio: base64, format: "s16le", rate: 16000}`
  → `{text}`。クライアントは得た文字列で `chat.send` する（文字起こしを見せてから送れる）。
- 申告: `client.speech.input = true` は現状ツールに影響しないが、ログと将来の
  「音声で話しかけられた」前提のプロンプト調整のために予約する。

#### 音声出力（TTS、リップシンク）

- `chat.send` の `Turn` に `speak: bool` を足す。真なら返答本文を**サーバーで合成**
  （既存の `TtsProvider`）し、`chat.progress {kind: "audio", seq, pcm: base64, rate, final}`
  で流す。返答の `content` は従来どおり返る（吹き出しと履歴のため）。
- 再生とリップシンクは **GUI 側**。再生中の PCM から音量・母音を推定して VRM の
  `aa` / `ih` / `ou` / `ee` / `oh` を動かす。表情（§8）とは別の層で、口以外の表情は保つ。
- 申告: `client.speech.output = { formats: ["s16le"], rates: [16000, 24000] }`。

#### カメラ・スクリーンショット

- **ユーザーが送るとき**: §6 の `attachments` に載せる（今回実装済みの経路）。
- **エージェントが取るとき**: ツール `capture(source: enum<申告された capture>)`。
  申告は `client.capture = ["screen", "camera"]`。エージェントからクライアントへの
  問い合わせとして、次の往復で行う。
  1. ツールが `chat.progress {kind: "capture_request", request_id, source}` を通知する。
  2. クライアントは撮影して `sapphire-agent.chat.capture_result {request_id, image?: {mime, data}, error?}` を呼ぶ。
  3. ツールは画像を `ContentPart::Image` としてツール結果に載せて返す。
     **30 秒**で応答がなければタイムアウトのツールエラー。`error`（拒否・失敗）もツールエラー。
- 撮影のたびに GUI で許可を求めるか、設定で常時許可するかは GUI 側の判断で、
  プロトコルは関与しない（拒否は `error` で返る）。
- この「通知で頼み、メソッドで答える」往復は、今後エージェントがクライアントに何かを
  頼む別のツールでも同じ形で使う。

## エラー処理

| 状況 | 振る舞い |
|---|---|
| エージェントが起動していない・切断 | 仮の表示＋吹き出しで伝え、再接続を続ける |
| manifest / read の失敗 | キャッシュにあるもので表示を続け、次の周期で再試行 |
| アップロード失敗・ハッシュ不一致 | 一時ファイルを捨て、設定ページにエラーを出す |
| VRM / VRMA の読み込み失敗 | モデルは仮の表示、アクションは申告から外す |
| 不正な表情・アクション名、2 回目の `play_action` | ツールエラーとして LLM に返す |

## テスト

- **cli**: `attachments` が `ContentPart::Image` になる・上限と mime の拒否、
  `client` の知らないキーを無視する、
  `client.avatar` の有無・`actions` の空でツールが出し分けられる、`play_action` の
  1 ターン 1 回、不正な名前のエラー、avatar IPC のパス検証、commit 時のハッシュ照合と
  置き換え、`set_manifest` の参照確認、`progress` に expression / action が載る。
- **core**: `progress` の各 kind が `ChatEvent` に解析される。
- **gui**: 表情の補間と neutral への復帰、まばたきのタイマー、ジェスチャー判定、
  manifest とキャッシュの差分計算を、bevy を起動しない単体テストで。
- **手動**: 透過・最前面・クリック透過・ドラッグ（Linux X11 / Wayland、Windows、macOS）。

## 作業の順番

1. fork: bevy_egui（egui 0.37）、bevy_vrm1（bevy 0.20）
2. `crates/sapphire-agent-gui` を作り、今の desktop のチャットを移す（振る舞いは変えない）
3. framework-gui 0.23 の `Settings` を組み込む
4. core / cli: `APP_API` 3、`Turn` / `ClientCaps`、`attachments`、`set_expression` /
   `play_action`、`progress` の拡張
5. cli: avatar IPC と `avatar/` の保存
6. gui: avatar の取得・キャッシュ、VRM 表示、表情・まばたき・視線・アクション
7. desktop: 透過メイトウィンドウ、クリック透過、ジェスチャー UI、吹き出し・履歴・メニュー
8. 「キャラクター」設定ページ（アップロード、アクションの編集）

## 範囲外

- モバイル版（`mobile/`、bevy でのサーバー内蔵、全画面 presenter、審査）
- キャラクターがパネルをよけて覗き込むモーション
- custom expression の申告（拡張点のみ用意）
- avatar の変更通知、heartbeat / autonomous ターンでの表情
- §12 の実装すべて: ウェイクワード・STT、TTS・リップシンク、`capture` ツールと
  クライアントへの問い合わせ、GUI の添付 UI（それぞれ別の設計書で）
