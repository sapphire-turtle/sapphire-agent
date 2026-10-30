# デスクトップマスコット（VRM / 2D 立ち絵のキャラクター）と GUI ライブラリ化

- **対象**: `desktop/`、新設 `crates/sapphire-agent-gui/`、`crates/sapphire-agent-core/`、
  `cli/src/tools/`、`cli/src/serve/`（chat IPC と avatar IPC）、`cliff.toml`、`release-plz.toml`
- **参照（framework）**: `sapphire-framework-gui` 0.23 の `Settings`（`settings.rs`）、
  `mobile/`（サーバー内蔵の型）
- **外部**: bevy 0.20、bevy_egui（fork）、bevy_vrm1 0.11（crates.io）
- **関連 issue**: #340（デフォルトキャラクター）
- **状態**: 実装中（plan 1・2 完了、§13・§14 を追加）

## なぜ

`sapphire-agent-desktop` は bevy + egui のチャット画面だけで、bevy を選んだ理由だった
3D キャラクター（`desktop/Cargo.toml` のコメント参照）はまだない。これを、デスクトップの上に
常駐する透過ウィンドウのキャラクター（デスクトップマスコット、英語では desktop companion）として
実装する。表示は VRM の 3D と、3D 描画が重い環境（GPU の無い VM など）向けの 2D 立ち絵の 2 通り。

呼び方: 文書では「デスクトップマスコット（desktop companion）」、コードの識別子とコメントでは
`companion` を使う。「デスクトップメイト」は既製品の Desktop Mate を指すので使わない。

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
| bevy_vrm1 | VRM 1.0 の expressions / look-at / spring bone / MToon / VRMA を持つ唯一の候補 | fork 不要。上流の 0.11.0（2026-10-29、上流 PR #73）が bevy 0.20 対応済みなので crates.io の `bevy_vrm1 = "0.11"` を使う。`sapphire-turtle/bevy_vrm1`（branch `bevy-0.20`）は、今後 bevy の更新が上流より先に進むときのためだけに残す |

`bevy_vrm`（unavi）0.5 は bevy 0.20 対応済みだが expressions と look-at が未対応なので採らない。
bevy_vrm1 は VRM 1.0 専用（0.x は読めない）。VRoid Studio の現行版は 1.0 を書き出せるので許容する。

### 3. クレート構成

```
crates/sapphire-agent-gui/   lib, publish = false（git 依存可）
  ├─ app        起動：runtime、FrameworkClient、フォント・テーマ、repaint
  ├─ chat/      チャット状態・bridge と egui ビュー（`crates/sapphire-agent-gui/src/chat/` に移した）
  │             （入力欄、吹き出し、履歴パネル、メニュー）
  ├─ avatar/    プラットフォーム非依存の bevy plugin（§14）
  │   ├─ assets      avatar IPC からの取得・アップロード・キャッシュ、素材の読み込み元
  │   ├─ state       表情・ポーズ・アクションの現在値と、neutral / 既定ポーズへ戻すタイマー
  │   ├─ mode        表示方式（自動 / 3D / 2D）の決定
  │   ├─ placeholder 未設定・失敗時の仮表示（シルエット）
  │   ├─ sprite      2D 立ち絵の描画（§13）
  │   └─ vrm         VRM の描画（bevy_vrm1）、まばたき、視線、VRMA、idle
  ├─ input      キャラクター上のジェスチャー → 入力欄 / 履歴 / メニュー
  ├─ settings   framework Settings の組み込み、「キャラクター」ページ
  └─ AgentGuiPlugin  上をまとめる。出し先（Presenter）は呼び出し側が渡す
desktop/                     bin：AgentGuiPlugin + 透過コンパニオンウィンドウの presenter
mobile/                      （将来）bin：AgentGuiPlugin + 全画面 presenter + 内蔵サーバー
```

- `avatar` と `chat` には `cfg(target_os)` を入れない。デスクトップ固有のもの
  （透過・最前面・クリック透過・ドラッグ移動）は `desktop/` の presenter だけが持つ。
- ライブラリには今回モバイルのコードを書かない。境界だけ作る。
- 純粋なロジック（表情・ポーズの状態と戻すタイマー、表示方式の決定、まばたきのタイマー、
  ジェスチャー判定、manifest の差分計算、2D の表示画像の解決）は
  bevy に依存しない関数・型にして単体テストできるようにする。

### 4. デスクトップのコンパニオンウィンドウ

- 1 つの透過・枠なし・最前面ウィンドウ。タスクバーに出さない。描画方式のカメラ（3D なら
  `Camera3d`、2D なら `Camera2d`）＋その上に egui。
- ウィンドウはキャラクターより大きめに取り、吹き出し・入力欄・履歴・メニューはその中に
  キャラクターへ重ねて出す。
- クリックを受けるのは、キャラクターの当たり判定（描画方式が書く `AvatarBounds`、§14）
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
      "actions": ["peace", "wave"],
      "poses": ["normal", "arms_crossed"]   // 2D のみ（§13）。省略可
    }
    // "speech": {...}, "capture": [...]  — §12、今回は未実装
  } }
```

- 申告を `open` ではなく `send` に載せるのは、同じセッションを desktop で開いた後で
  CLI の `chat` から再開しうるため。ターン単位ならサーバーは状態を持たない。
- `client.avatar` があるターンだけ LLM のツール一覧に `set_expression` を加え、
  `actions` が空でなければ `play_action` を、`poses` が空でなければ `set_pose` も加える。
  申告するのは、その端末が実際に表示している描画方式（§14）で読み込めたものだけ。heartbeat / autonomous / Discord / Matrix の
  ターンには出ない。

#### 添付（今回実装する）

`attachments` はユーザーのメッセージに付ける画像。各要素は `{mime, data}`（base64）で、
`image/png` / `image/jpeg` / `image/webp` を受ける。サーバーは既存の `ContentPart::Image`
としてメッセージに載せ、`image_cache` の扱い（sha256 で外に逃がす）にそのまま乗せる。
1 件 20 MiB・1 ターン 8 件まで。超過や未対応の mime は `send` のエラーにする
（ターンは始めない）。GUI 側の添付 UI（ファイル選択・撮影）は今回作らない。
- 今回クライアントが申告する `expressions` は VRM 1.0 の感情プリセット 6 種の中から選ぶ
  （3D はモデルにあるもの、2D は §13 の規則で差分があるもの）。
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

#### `set_pose`

```
set_pose(pose: enum<申告された poses>)
```

- 説明文: 立ち姿（ポーズ）を切り替える。次に変えるまで保持される。表情とは独立。
  1 ターンに何度呼んでもよい。
- 実行すると `progress` 通知を送り、`"ok"` を返す。
- 一覧にない名前はツールエラー（有効な一覧を添える）。
- 今は 2D だけが申告する。3D で静止ポーズを VRMA として持つ拡張は、申告を足すだけで済む。

#### 通知

```jsonc
// sapphire-agent.chat.progress（既存の tool_start / tool_end に加えて）
{ "session_id": "...", "kind": "expression", "expression": "happy", "intensity": 0.8 }
{ "session_id": "...", "kind": "action", "action": "peace" }
{ "session_id": "...", "kind": "pose", "pose": "arms_crossed" }
```

`set_expression` / `play_action` / `set_pose` 自体の `tool_start` / `tool_end` も従来どおり流れる。
GUI はこの 3 つを「考え中」表示から除外する。

#### core の API

```rust
pub enum ChatEvent {
    Tool(ToolEvent),
    Expression { name: String, intensity: f32 },
    Action { name: String },
    Pose { name: String },
}

#[derive(Default)]
pub struct ClientCaps { pub avatar: Option<AvatarCaps> /* speech, capture は §12 で追加 */ }
pub struct AvatarCaps { pub expressions: Vec<String>, pub actions: Vec<String>, pub poses: Vec<String> }
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

```toml
# <workspace>/avatar/avatar.toml — [vrm] と [sprite] はどちらも省略可、両方あってよい
[vrm]
model = "vrm/model.vrm"
idle  = "vrm/idle.vrma"            # 省略可
[vrm.actions]
peace = "vrm/peace.vrma"

[sprite]
default_pose = "normal"            # poses のどれか（必須）
[sprite.poses.normal]
base = "sprite/normal.png"         # このポーズの neutral（必須）
[sprite.poses.normal.expressions]
happy = "sprite/normal-happy.png"
sad   = "sprite/normal-sad.png"
[sprite.poses.arms_crossed]
base = "sprite/arms.png"
```

ファイルの置き方（`vrm/`・`sprite/` など）は慣習で、強制しない。`avatar.toml` に書いたパスだけが
意味を持つ。この形は APP_API 3 が未リリースのうちに旧形（トップレベルの `model` / `idle` /
`actions`）から組み替えたもので、旧形の読み込みは持たない。

core の型:

```rust
pub struct AvatarConfig { pub vrm: Option<VrmConfig>, pub sprite: Option<SpriteConfig> }
pub struct VrmConfig { pub model: String, pub idle: Option<String>, pub actions: BTreeMap<String, String> }
pub struct SpriteConfig { pub default_pose: String, pub poses: BTreeMap<String, SpritePose> }
pub struct SpritePose { pub base: String, pub expressions: BTreeMap<String, String> }
```

framework の indexer は markdown 系（`md` / `markdown` / `txt` / `rst` / `org`）と
画像拡張子しか索引しないので、`.vrm` / `.vrma` / `.toml` は索引に入らない。
除外の実装は不要。

#### IPC メソッド

| メソッド | 引数 → 結果 |
|---|---|
| `sapphire-agent.avatar.manifest` | `{}` → `{vrm?, sprite?, files: [{path, size, sha256}]}`（`vrm` / `sprite` は `avatar.toml` と同じ形） |
| `sapphire-agent.avatar.read` | `{path, offset, len}`（len ≤ 1 MiB）→ `{data: base64, eof}` |
| `sapphire-agent.avatar.write_begin` | `{path, size, sha256}` → `{upload_id}` |
| `sapphire-agent.avatar.write_chunk` | `{upload_id, offset, data: base64}` → `{}` |
| `sapphire-agent.avatar.write_commit` | `{upload_id}` → `{}`（ハッシュ照合後に rename で置き換え） |
| `sapphire-agent.avatar.set_manifest` | `{vrm?, sprite?}` → `{}`（`avatar.toml` を書き直す） |
| `sapphire-agent.avatar.remove` | `{path}` → `{}` |

- `path` は `avatar/` からの相対で、`/` 区切り。`..`・絶対パス・`avatar.toml` 直接は拒否する。
- 1 ファイル 256 MiB まで。
- 書き込みは `avatar/` の外の一時領域に置き、commit で照合してから置き換える。
  未完成のファイルが同期に乗らないようにするため。放置されたアップロードは、サーバー側の `PeerHandle` に切断の通知が無いため、最後の書き込みから 10 分で捨てる（次の `write_begin` のときに掃除する）。
  別ファイルシステムで rename できないときは、`avatar/` 内の隠しファイル `.<名前>.<upload_id>.uploading` にコピーしてから rename する。隠しファイルは manifest に載せない。
- `set_manifest` は、参照するファイル（`vrm` と `sprite` の全パス）が存在しなければ拒否する。
  `sprite` は `default_pose` が `poses` に無い・`poses` が空でも拒否する。名前（アクション・
  ポーズ・表情）は空文字を拒否する。`remove` は、どちらかが参照しているファイルを拒否する。

#### GUI 側

- 接続時・自分のアップロード後・5 分ごとに `manifest` を取り、sha256 が手元のキャッシュ
  （GUI のキャッシュディレクトリ、sha256 名）に無いファイルだけ `read` で取る。
  他端末から同期で届いた変更も次の取得で反映される。変更通知は今回作らない。
- bevy の読み込みはキャッシュのファイルから行う。
- 素材の読み込み元は `assets` の 1 か所で差し替えられるようにする。デフォルトキャラクター
  （#340）を GUI に同梱する場合、`avatar/` が空ならそちらを使う、をここに足す。
- 「キャラクター」設定ページ: 共通部分（表示方式、戻すまでの秒数、操作ボタンの表示）の下に、
  描画方式ごとの欄を置く。
  - VRM の欄: `.vrm` を選ぶとアップロードして `set_manifest`、アクション名と `.vrma` の組を
    追加・削除・改名、idle の指定。
  - 2D の欄: ポーズの追加・削除・改名、ポーズごとの `base` と表情差分の画像の指定、
    `default_pose` の指定。
  - どちらの欄も、自分の側（`vrm` / `sprite`）だけを書き換え、もう一方はそのまま送り返す。
- `send` で申告するものは、表示中の描画方式で読み込みに成功したものだけ。

#### モバイル

サーバーを内蔵するので、同じメソッドを同じプロセス内の IPC carrier 越しに呼ぶ。

### 8. キャラクターの振る舞い

- **表情**: 受けた目標へ約 0.2 秒で補間する（2D はクロスフェード）。次の `set_expression` まで
  保持し、ターンの返答が終わってから **30 秒**（GUI ローカル設定）で `neutral` へゆっくり戻す。
- **ポーズ**（2D）: 次の `set_pose` まで保持し、表情と同じタイマーで `default_pose` へ戻す。
  切り替えは約 0.2 秒のクロスフェード。
- **まばたき**（3D）: 2〜6 秒のランダム間隔。表情の `overrideBlink` を尊重する。2D はまばたき・
  口パクを持たない。
- **視線**（3D）: デスクトップはカーソル、モバイル（将来）は正面。
- **揺れもの**（3D）: bevy_vrm1 の spring bone に任せる。
- **idle**（3D）: `idle` があればループ再生、なければ静止ポーズ。
- **アクション**（3D）: VRMA を一度再生して idle に戻る。再生中に来た次のアクションは無視する。
- **未設定・失敗**: 仮の表示（シルエット）。クリックで設定の「キャラクター」ページを開く。
  失敗の理由は吹き出しで出す。

### 9. GUI ローカル設定（同期しない）

表示方式（自動 / 3D / 2D、§14）、表情・ポーズを戻すまでの秒数、操作ボタンの表示、
コンパニオンウィンドウの位置。端末ごとの好みなので
サーバーには置かない。

### 10. バージョン

- `APP_API` を 2 → **3**。変更履歴: 「3: `chat.send` に `client`（`avatar`）と
  `attachments`、`chat.progress` に `expression` / `action` / `pose`、`set_pose`、
  `sapphire-agent.avatar.*` を追加」。3 は未リリースのうちに `pose` と `avatar.toml` の
  組み替え（§7）を足したので、番号は上げずに履歴の文言だけ直す。
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

### 13. 2D 立ち絵の表示

3D 描画が重い環境（GPU の無い VM など）向け。素材はユーザーが用意する立ち絵（ポーズごと）と
表情差分。口パク・まばたき・Live2D のような細かい動きは持たない。

- **キャンバス**: 1 つの `[sprite]` の全画像は同じピクセルサイズにし、同じ原点で重ねる。
  そのため表情差分は、全身入りの画像でも、顔だけを残した透過画像でもよい。サイズが揃わない
  ときは読み込みエラーにして設定ページに出す（仮表示になる）。
- **形式**: PNG・WebP。
- **表情の名前**: `expressions` のキーは VRM 1.0 の感情プリセットのうち `neutral` 以外の 5 種
  （`happy` / `angry` / `sad` / `relaxed` / `surprised`）に限る（`set_manifest` が拒否する）。
  3D と同じ名前にそろえ、デフォルトキャラクター（#340）の素材とも対応させるため。
- **表示する画像**: いまのポーズ `p` と表情 `e` について、
  1. `p` の `base` を描く
  2. `e` が `neutral` でなく、`p.expressions[e]` があれば、その上に重ねる
  3. 無ければ `base` だけ（`neutral` 扱い）
- **申告**: `poses` は `[sprite.poses]` の全キー。`expressions` は `neutral` と、どれか 1 つの
  ポーズに差分がある表情。`actions` は申告しない（2D にアクションは無い）。
- `intensity` は使わない。
- **描画**: `Camera2d` と bevy の `Sprite`（ベース・差分の 2 枚）。表示サイズはウィンドウの
  高さに合わせ、縦横比を保つ。当たり判定（`AvatarBounds`）は画像の矩形。不透明部分だけに
  絞るのは将来。

### 14. 描画方式の境界と並行開発

**表示方式の決定**（`avatar/mode`、純粋関数）: GUI ローカル設定の 自動 / 3D / 2D と、
`avatar.toml` の `[vrm]` / `[sprite]` の有無から決める。

| 設定 | `[vrm]` あり | `[sprite]` あり | 結果 |
|---|---|---|---|
| 自動 | ○ | どちらでも | 3D |
| 自動 | × | ○ | 2D |
| 3D / 2D | 指定した側がある | | 指定した側 |
| どれでも | 該当なし | | 仮表示 |

**描画方式の約束**: 3D（`avatar/vrm`）と 2D（`avatar/sprite`）はそれぞれ 1 つの bevy plugin で、
`mode` で選ばれたものだけが動く（run condition）。仮表示（`avatar/placeholder`）も同じ約束に従う。

- 読む: `AvatarState`（いまの表情・ポーズ・アクション、`avatar/state`）、読み込み済みの
  `AvatarConfig`、キャッシュのファイルパス（`avatar/assets`）。
- 書く: `AvatarCaps`（読み込めた表情・ポーズ・アクション。chat の bridge が `send` のたびに
  読んで `client.avatar` に載せる）、`AvatarBounds`（画面上のキャラクターの矩形。ジェスチャー
  判定とクリック透過が使う）、読み込みエラー（設定ページと吹き出しへ）。
- 共通部分は描画方式の中身を知らず、描画方式どうしも互いを知らない。カメラは描画方式が
  自分で出し、選ばれなくなったら消す。

**並行開発**: 共通基盤（plan 3）の後、2D（plan 4-2D）と 3D（plan 4-3D）を別のホストで並行して
進める。触るのは各自の `avatar/sprite` / `avatar/vrm` と、設定ページの自分の欄だけにする。
共通部分に手を入れる必要が出たら、その変更だけを先に別の PR で main に入れ、もう一方は
それを取り込んでから続ける。

## エラー処理

| 状況 | 振る舞い |
|---|---|
| エージェントが起動していない・切断 | 仮の表示＋吹き出しで伝え、再接続を続ける |
| manifest / read の失敗 | キャッシュにあるもので表示を続け、次の周期で再試行 |
| アップロード失敗・ハッシュ不一致 | 一時ファイルを捨て、設定ページにエラーを出す |
| VRM / VRMA の読み込み失敗 | モデルは仮の表示、アクションは申告から外す |
| 立ち絵の読み込み失敗・キャンバスサイズ不一致 | `base` が読めないポーズは申告から外す。既定ポーズが読めなければ仮の表示。差分が読めない表情は申告から外す |
| 選んだ表示方式の定義が無い | 仮の表示（クリックで「キャラクター」ページ） |
| 不正な表情・アクション・ポーズ名、2 回目の `play_action` | ツールエラーとして LLM に返す |

## テスト

- **cli**: `attachments` が `ContentPart::Image` になる・上限と mime の拒否、
  `client` の知らないキーを無視する、
  `client.avatar` の有無・`actions` / `poses` の空でツールが出し分けられる、`play_action` の
  1 ターン 1 回、不正な名前のエラー、avatar IPC のパス検証、commit 時のハッシュ照合と
  置き換え、`set_manifest` の参照確認（`vrm` / `sprite` の全パス、`default_pose`）、
  `progress` に expression / action / pose が載る。
- **core**: `progress` の各 kind が `ChatEvent` に解析される。
- **gui**: 表情・ポーズの状態と戻すタイマー、表示方式の決定（§14 の表）、2D の表示画像の解決
  と申告の組み立て（§13）、まばたきのタイマー、ジェスチャー判定、manifest とキャッシュの
  差分計算を、bevy を起動しない単体テストで。
- **手動**: 透過・最前面・クリック透過・ドラッグ（Linux X11 / Wayland、Windows、macOS）。

## 作業の順番

1. fork: bevy_egui（egui 0.37）。bevy_vrm1 は上流 0.11 を使うので fork 不要
2. `crates/sapphire-agent-gui` を作り、今の desktop のチャットを移す（振る舞いは変えない）
3. framework-gui 0.23 の `Settings` を組み込む
4. core / cli: `APP_API` 3、`Turn` / `ClientCaps`、`attachments`、`set_expression` /
   `play_action`、`progress` の拡張
5. cli: avatar IPC と `avatar/` の保存
6. **plan 3（共通基盤）**:
   - core / cli: `avatar.toml` の組み替え（§7）、`poses` と `set_pose`、`progress` の `pose`
   - gui: `avatar/assets`（取得・キャッシュ）、`state`・`mode`・`AvatarCaps`・`AvatarBounds`・
     `placeholder`
   - gui: ジェスチャー判定、吹き出し・入力欄・履歴・メニュー・操作ボタン
   - desktop: 透過コンパニオンウィンドウ、クリック透過、ドラッグ、位置の保存
   - 「キャラクター」設定ページの外枠と共通項目
7. 並行（§14）:
   - **plan 4-2D**: `avatar/sprite` と設定ページの 2D の欄
   - **plan 4-3D**（GPU のあるホスト）: `avatar/vrm`（VRM 表示、表情・まばたき・視線・VRMA・
     idle）と設定ページの VRM の欄

## 範囲外

- モバイル版（`mobile/`、bevy でのサーバー内蔵、全画面 presenter、審査）
- キャラクターがパネルをよけて覗き込むモーション
- custom expression の申告（拡張点のみ用意）
- デフォルトキャラクターと初回起動の流れ（#340）。素材の読み込み元の差し替え口だけ用意する
- 3D の静止ポーズ（`set_pose` の 3D 対応）、2D のまばたき・口パク、2D の当たり判定を不透明部分に絞ること
- avatar の変更通知、heartbeat / autonomous ターンでの表情
- §12 の実装すべて: ウェイクワード・STT、TTS・リップシンク、`capture` ツールと
  クライアントへの問い合わせ、GUI の添付 UI（それぞれ別の設計書で）
