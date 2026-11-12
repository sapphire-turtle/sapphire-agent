# チャネル添付保存・画像読み取り 実装計画（#395・#396）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discord / Matrix の添付原本を7日キャッシュし、許可されたチャネル会話から保存する。保存済み画像を agent 側のパスからモデルに返す。

**Architecture:** 画像専用 `Attachment` と `ImageCache` を保ち、workspace-external な `AttachmentStore` を別に追加。チャネルで原本保存後に画像だけ `prepare_image`。共有 ToolSet の新ツールは host gate / offered gate / owner scope / ACP 拒否で制限する。

**Tech Stack:** Rust、tokio、serde_json、uuid v7、reqwest、matrix-sdk 0.19、既存 sapphire-framework 0.28 の WorkspaceState バイナリ API。

**Spec:** `docs/superpowers/specs/2026-11-12-channel-attachments-image-read-design.md`

## File Structure

| ファイル | 役割 |
|---|---|
| `crates/sapphire-agent-server/src/attachment_store.rs`（新規） | owner、メタデータ、原本ストア、7日掃除、容量・時計を注入したテスト |
| `crates/sapphire-agent-server/src/tools/attachment_tools.rs`（新規） | owner task-local、attachment_save、image_read と単体テスト |
| `crates/sapphire-agent-server/src/channel/mod.rs` | IncomingMessage の owner、受信制限定数、原本保存・注記・画像準備の共通処理 |
| `crates/sapphire-agent-server/src/channel/discord.rs` | 全添付の有界取得、ストア注入、owner 作成 |
| `crates/sapphire-agent-server/src/channel/matrix.rs` | 4種 media の取得、ストア注入、owner 作成 |
| `crates/sapphire-agent-server/src/agent.rs` | owner bind、添付ツール表示、offered-tool 検査、fixture |
| `crates/sapphire-agent-server/src/tools/mod.rs` | 新モジュール、image_read の登録、kind 表 |
| `crates/sapphire-agent-server/src/tools/builtin_tools.rs` | sensitive write guard の crate 内共有、IncomingMessage を持つ fixture |
| `crates/sapphire-agent-server/src/tools/policy.rs` | 新2ツールの host gate とテスト |
| `crates/sapphire-agent-server/src/tools/subagent.rs` | 子実行で owner を消す |
| `crates/sapphire-agent-server/src/serve/mod.rs` | 非チャネルの attachment_save 非表示、ACP の image_read 非表示、テスト |
| `crates/sapphire-agent-server/src/consult.rs` | IncomingMessage fixture の owner None |
| `crates/sapphire-agent-server/src/lib.rs` | module、ストア起動、掃除タスク、チャネル・ツール配線と終了 |
| `docs/sapphire-agent-tools.md` | ツール、取得/保持/権限/制限の利用者説明 |
| `crates/sapphire-agent-server/config.example.toml` | host_access の対象2ツールと一時キャッシュの説明 |

## Global Constraints

- フミトの決定（2026-11-12）: 非画像も受信時に自動取得。未保存原本は7日。保存済みファイルを掃除しない。
- 原本32 MiB、8件/投稿、64 MiB/投稿、ストア1 GiB。7日=604800000 ms、掃除間隔3600秒、取得timeout60秒。バイト上限の比較は `>`、期限の比較は `now >= expires_at_ms`。
- 画像の5 MiB、1568pxと縮小品質は現行 prepare_image に任せる。別実装を作らない。
- 原本ストアを image_cache と共有しない。URL・鍵を永続化しない。ストア無効時も既存画像入力を壊さない。
- `attachment_save` は Edit / チャネル owner 必須、`image_read` は Read。両方 host_access 必須、ACP で拒否。host gate を bypass する新しい許可フラグは作らない。
- ACP/RPC/IPC の wire、依存バージョン、file_read の契約は変更しない。
- Path 表示は `workspace::slash_path`。POSIX 固有テストだけ `#[cfg(unix)]`。
- 既定検証は `cargo test -p sapphire-agent-server`。最終 `cargo test --workspace`、`cargo clippy --workspace --all-targets`。失敗を既知扱いで黙殺せずレポートに残す。
- 各コミット末尾: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。

## Review Focus

原本と縮小画像の取り違え、別会話の ID、subagent への scope 漏れ、host/ACP gate、再起動・期限境界、capacity check の競合、Matrix の非画像が return で消えないことを優先する。

### Task 1: 原本ストア（単独でテストできる層）

**Files:** Create `crates/sapphire-agent-server/src/attachment_store.rs`; Modify `crates/sapphire-agent-server/src/lib.rs`（module 宣言のみ）。

- [ ] **Step 1: 次の型と定数を定義する**

```rust
pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const STORE_CAP_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentOwner {
    pub platform: String,
    pub room_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredAttachment {
    pub version: u32,
    pub id: String,
    pub owner: AttachmentOwner,
    pub message_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: u64,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
}
```

`AttachmentStore` は dir: PathBuf、gate: std::sync::Mutex<()>、cap: u64 を持つ。公開メソッドは同期 API とし呼ぶ側で spawn_blocking。`open(dir: PathBuf) -> Result<Arc<Self>>`、`default_dir(root: &Path) -> Result<PathBuf>`、`insert(owner: AttachmentOwner, message_id: String, filename: String, media_type: String, bytes: &[u8], now_ms: u64) -> Result<StoredAttachment>`、`get(owner: &AttachmentOwner, id: &str, now_ms: u64) -> Result<(StoredAttachment, Vec<u8>)>`、`sweep(now_ms: u64) -> Result<()>`。テスト用 `open_with_cap` は cfg(test)。時計は引数にして sleep を使わない。

- [ ] **Step 2: RED テストを書く**

`original_survives_reopen`、`expiry_is_inclusive_and_reads_do_not_extend_it`、`owner_is_platform_room_and_thread`、`rejects_noncanonical_uuid_and_traversal`、`capacity_refuses_new_without_evicting_live`、`concurrent_inserts_cannot_exceed_cap`、`sweep_only_touches_generated_files`、`orphan_and_corrupt_entries_are_cleaned`、`future_version_is_preserved_and_counted`。全て tempfile を使用。`persisted_metadata_contains_no_source_secrets` では URL・Matrix 暗号鍵の sentinel を持つ受信 fixture から保存まで通し、ストアの全 JSON に元 URL、署名 query、鍵 sentinel が含まれないことを assert する（受信 fixture と接続する検証は Task 4 で完成させる）。

```rust
#[test]
fn expiry_is_inclusive_and_reads_do_not_extend_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::open(dir.path().to_owned()).unwrap();
    let owner = AttachmentOwner {
        platform: "discord".into(), room_id: "7".into(), thread_id: None,
    };
    let meta = store.insert(owner.clone(), "m".into(), "x.pdf".into(),
        "application/pdf".into(), b"original", 1000).unwrap();
    assert!(store.get(&owner, &meta.id, 1000 + RETENTION_MS - 1).is_ok());
    assert!(store.get(&owner, &meta.id, 1000 + RETENTION_MS).is_err());
    store.sweep(1000 + RETENTION_MS).unwrap();
    assert!(!dir.path().join(format!("{}.bin", meta.id)).exists());
}
```

Run: `cargo test -p sapphire-agent-server attachment_store`。初回は未実装エラーで失敗を確認。

- [ ] **Step 3: 実装する**

default_dir は canonical root の lossless OS bytes（spec §1.2）→ Sha256 → hex、`dirs::cache_dir().context(...)` の attachments 配下。insert は Mutex 内で sweep、実サイズ32 MiBを検査、UUID v7発行、serde_json::to_vec、全ファイルの長さと追加分で checked_add、cap 判定。`.bin.tmp` / `.json.tmp` を create_new で書き drop、rename で確定。部分失敗時は当該 ID の生成ファイルを掃除してエラーにする。

get は UUID 正規形、一致 owner、version=1、期限、is_file（symlink_metadata）、実サイズと metadata.bytes の一致を確認。読取は32 MiB+1の有界読み。別 owner・不存在・期限切れの全拒否は、正確に `attachment unavailable in this conversation or expired` に統一する。テスト `unavailable_errors_are_identical` で3条件の `err.to_string()` がこの文言と完全一致することを検証し、ID・owner・期限・パスを含む context を追加しない。sweep は UUID に合う既知 suffix の通常ファイルのみ処理。未知名・ディレクトリ・symlink は辿らず残す（容量には通常ファイルのみ算入、未知名の通常ファイルも含む）。未来 version の json と対応 bin は残す。Mutex 内から呼ぶ掃除本体は別 private 関数にして再ロックしない。

- [ ] **Step 4: GREEN とコミット**

Run: `cargo test -p sapphire-agent-server attachment_store` → 全該当テスト成功。`cargo fmt --all -- --check` → exit 0。

Commit: `feat(channel): cache attachment originals for seven days`。

### Task 2: agent 側の画像読み取りツール

**Files:** Create `crates/sapphire-agent-server/src/tools/attachment_tools.rs`; Modify `crates/sapphire-agent-server/src/tools/mod.rs`, `crates/sapphire-agent-server/src/tools/policy.rs`, `crates/sapphire-agent-server/src/serve/mod.rs`。

- [ ] **Step 1: RED とツール schema**

`ImageReadTool::new(Arc<WorkspaceState>)`。input_schema は次で固定:

```rust
serde_json::json!({
    "type": "object",
    "properties": { "path": { "type": "string",
        "description": "Image path on this agent's machine: absolute, ~/..., or workspace-relative." } },
    "required": ["path"],
    "additionalProperties": false
})
```

説明に JPEG/PNG/GIF/WebP、1画像、32 MiB入力、5 MiB超縮小、host_access必須、ACP非対応を明記。次のテストを実装して先に落とす: `image_read_sniffs_not_extension`、`image_read_shrinks_without_mutating_original`、`image_read_rejects_oversize_and_nonimage`、`image_read_rejects_directory`、`image_read_refuses_acp_without_host_fallback`。テスト workspace は builtin_tools の test_workspace パターンを使用。大きな画像 fixture はランダム風 RGB から PNG エンコードして5 MiB超を assert してから呼ぶ。特殊ファイル/FIFO は cfg(unix)。

Run: `cargo test -p sapphire-agent-server image_read` → RED。

- [ ] **Step 2: 有界ファイル読みと画像出力を実装**

`expand_path`、`state.resolve_path(&path)?.as_path()` の解決済み PathBuf を使う。通常ファイル metadata 検査後に open し、file.metadata も再検査。/dev と /proc は Path::starts_with で解決後にも検査。blocking closure 内の読み取り部分:

```rust
use std::io::Read;
let mut bytes = Vec::new();
file.take(crate::channel::MAX_DOWNLOAD_BYTES as u64 + 1)
    .read_to_end(&mut bytes)?;
anyhow::ensure!(bytes.len() <= crate::channel::MAX_DOWNLOAD_BYTES,
    "image exceeds the 32 MiB input limit");
let original_len = bytes.len();
let prepared = crate::channel::prepare_image(bytes)
    .map_err(anyhow::Error::msg)?;
```

prepare_image が失敗なら JPEG/PNG/GIF/WebP だけ対応、SVG は file_read でテキストとして読める旨を context に足す。`execute_full` の冒頭で current_acp_client().is_none を ensure。spawn_blocking の戻りから BASE64_STANDARD.encode して images 1件、text に slash_path と original_len と準備後 MIME、`original_len > MAX_ATTACHMENT_BYTES` の縮小有無を含める。`execute` は `self.execute_full(input).await?.text` を返す。

- [ ] **Step 3: gate と登録**

policy::HOST_TOOLS に `image_read` を追加。serve::visible_tool_predicate は has_client && name=="image_read" なら false（routed_to_client による host gate 免除よりも優先）。default_tool_set に登録し every_tool_declares_its_kind の期待値に追加。表形式の visibility テストで host off / host on / ACP の組合せを固定する。host gate の説明「seven」等を実際の数と整合させる。

- [ ] **Step 4: GREEN とコミット**

Run: `cargo test -p sapphire-agent-server image_read`、`cargo test -p sapphire-agent-server tools::` → exit 0。

Commit: `feat(tools): read local image files as multimodal results`。

### Task 3: 原本保存ツールと所有者 scope

**Files:** Modify `crates/sapphire-agent-server/src/tools/attachment_tools.rs`, `crates/sapphire-agent-server/src/tools/builtin_tools.rs`, `crates/sapphire-agent-server/src/tools/mod.rs`, `crates/sapphire-agent-server/src/tools/policy.rs`, `crates/sapphire-agent-server/src/tools/subagent.rs`, `crates/sapphire-agent-server/src/serve/mod.rs`。

- [ ] **Step 1: scope と schema を作り RED テストを書く**

```rust
tokio::task_local! {
    static ATTACHMENT_OWNER: Option<crate::attachment_store::AttachmentOwner>;
}
pub fn current_attachment_owner() -> Option<crate::attachment_store::AttachmentOwner> {
    ATTACHMENT_OWNER.try_with(Clone::clone).ok().flatten()
}
pub async fn scope_attachment_owner<F: std::future::Future>(
    owner: Option<crate::attachment_store::AttachmentOwner>, future: F,
) -> F::Output {
    ATTACHMENT_OWNER.scope(owner, future).await
}
```

`AttachmentSaveTool::new(store: Arc<AttachmentStore>, state: Arc<WorkspaceState>)`。schema は object、必須 string `attachment_id` と `path`、additionalProperties=false。kind Edit。説明は原本・上書き・7日・同会話・host_access・agent側限定。

テスト: `attachment_save_copies_original_and_overwrites`、`attachment_save_requires_owner`、`attachment_save_rejects_foreign_owner_and_expired`、`attachment_save_refuses_sensitive_and_api_keys`、`attachment_save_refuses_acp`、`owner_scope_is_cleared_in_child`。scope の前後で current_attachment_owner が None であることも確認する。

Run: `cargo test -p sapphire-agent-server attachment_save` → RED。

- [ ] **Step 2: 保存を実装する**

execute は ACP 拒否 → current owner 必須 → required fields 検証 → expand_path → 共用 sensitive guard → spawn_blocking で store.get と state.write_bytes。既存 `refuse_if_sensitive` の visibility のみ pub(crate) にする（guard の判定は変更しない）。現在時刻は chrono::Utc::now().timestamp_millis().max(0) as u64。get 内で期限判定する。`attachment_save_unavailable_errors_are_identical` では別 owner・不存在・期限切れをツールから実行し、3件とも `attachment unavailable in this conversation or expired` と完全一致することを assert する（ツール層で context を足して漏洩しない）。成功文は `Saved attachment {id} to {slash_path} ({len} original bytes).`。

policy::HOST_TOOLS に attachment_save を追加。serve::visible_tool_predicate は attachment_save を常に false（Agent は Task 5 で独自に let-back-in）。subagent の子 TurnLoop 全体を `scope_attachment_owner(None, future)` で包む。ACP client scope と timer scope は維持する。

- [ ] **Step 3: GREEN とコミット**

Run: `cargo test -p sapphire-agent-server attachment_save`、`cargo test -p sapphire-agent-server subagent`、`cargo test -p sapphire-agent-server tools::policy` → exit 0。

Commit: `feat(tools): save conversation-owned attachment originals`。

### Task 4: Discord / Matrix の全メディア取得

**Files:** Modify `crates/sapphire-agent-server/src/channel/mod.rs`, `crates/sapphire-agent-server/src/channel/discord.rs`, `crates/sapphire-agent-server/src/channel/matrix.rs`。

- [ ] **Step 1: 共通受信関数と互換コンストラクタ**

IncomingMessage は Task 5 まで変更しない。DiscordChannel / MatrixChannel に Option<Arc<AttachmentStore>> を追加し、既存 new は None、`with_attachment_store(mut self, store: Option<Arc<AttachmentStore>>) -> Self` builder を足す。DiscordHandler にもストアを渡す。

channel/mod.rs に `MAX_RECEIVED_ATTACHMENTS: usize = 8`、`MAX_MESSAGE_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024` を追加。`cache_and_prepare_attachment(store, owner, message_id, filename, declared_mime, bytes) -> (Option<Attachment>, Vec<String>)` を async 共通処理として作る。引数は owned（store は Option<Arc<...>>）。store.insert と prepare_image は blocking task。bytes を insert の原本として使い、画像変換には別コピーを渡す。store None / insert エラーでも sniff が画像なら画像準備を試す。

注記は serde_json::to_string で filename/id/MIME を引用。成功注記のフォーマットを次に固定する（`meta` は insert の結果）:

```rust
let status = if inline_shown {
    if resized {
        "cached original, image shown inline; inline image may be resized; saved bytes are original"
    } else {
        "cached original, image shown inline"
    }
} else {
    "cached original, not viewed inline"
};
let note = format!(
    "[Attachment id={} filename={} media_type={} bytes={} expires_at_ms={}; {}. Use attachment_save to save it.]",
    serde_json::to_string(&meta.id)?,
    serde_json::to_string(&meta.filename)?,
    serde_json::to_string(&meta.media_type)?,
    meta.bytes, meta.expires_at_ms, status,
);
```

`inline_shown` は prepare_image 成功、`resized` は原本サイズが MAX_ATTACHMENT_BYTES を超えて画像準備が成功した場合。非画像の保存成功は unsupported エラーにしない。画像 preparation 失敗も保存成功 ID を残し、別の失敗注記を添える。

- [ ] **Step 2: RED テスト**

共通処理: `cached_png_original_is_not_resized_copy`、`pdf_is_cached_but_not_inline`、`cache_failure_still_passes_inline_image`、`notes_escape_newlines_and_quotes`。Discord の screen_attachment テストは「非画像は受理」「unknown MIME は受理」「32 MiB超拒否」に置換。Matrix は JSON で Image/File/Audio/Video を deserialize し、caption / filename / source / info 抽出の pure helper をテストする。

成功注記の `success_notes_include_save_contract` テストは非画像・通常画像・縮小画像・画像準備失敗の4ケースで、動的 ID と期限を meta から埋めた期待文字列との完全一致を assert する。bytes は原本サイズ、expires_at_ms は受信時刻+RETENTION_MS、全ケースに保存案内、画像成功に inline 状態、縮小時に原本説明があることを固定する。失敗注記だけで保存可能 ID が消えないことも検証する。Task 1 の sentinel テストをここで完成させ、Matrix の encrypted source に鍵を与えても保存 JSON に残らないことを検証する。

Run: `cargo test -p sapphire-agent-server channel::` → RED。

- [ ] **Step 3: Discord の取得を差し替える**

download_image_attachments を download_attachments に改名。イベントの添付だけ順に処理し先頭8件、合計64 MiBを守る。事前スクリーンは size だけ。reqwest client は handler に保持して再利用。URL は msg.attachments の url を使い、request.send → error_for_status → chunk() loop を timeout(60s) に包む。既知 Content-Length が残量を超えたら即拒否、chunk の加算は checked_add してファイル/投稿残量を超えれば中断。失敗は注記を残して continue。原本を共通関数へ渡す。owner platform="discord"、room_id=msg.channel_id、thread_id=None（既存 Discord routing と同じ）。

- [ ] **Step 4: Matrix の取得を一般化する**

`download_matrix_image` を media.source、optional size を取る download_matrix_media に置換。SDK `get_media_content(&request, false)` を60秒 timeoutで呼び、size を u64 のまま比較（usize cast で切り捨てない）。返る Vec も検査。MessageType::Image / File / Audio / Video から filename() / caption() / source / info.mimetype / info.size を抽出する helper を作る。Text の reply fallback と relation はそのまま。owner platform="matrix"、room と既存抽出 thread_id。対応 media は共通関数へ渡し、失敗も本文注記にする。その他 msgtype は従来どおり無視。

Matrix は1イベント1添付なので64 MiB合計は32 MiB単件で満たす。SDKが全量バッファする制約をコメントに明記し、ストリーム上限を保証したと書かない。

- [ ] **Step 5: GREEN とコミット**

Run: `cargo test -p sapphire-agent-server channel::` → 全成功。ローカルHTTPの Discord download helper テストで申告無し巨大応答、chunk超過、HTTP失敗を追加してネットワーク実アクセス無しで通す。timeout は helper の timeout 引数をテスト用に短くし sleep 60秒を避ける。

Commit: `feat(channel): receive and cache non-image attachments`。

### Task 5: 起動・会話所有者・掃除を配線する

**Files:** Modify `crates/sapphire-agent-server/src/lib.rs`, `crates/sapphire-agent-server/src/channel/mod.rs`, `crates/sapphire-agent-server/src/channel/discord.rs`, `crates/sapphire-agent-server/src/channel/matrix.rs`, `crates/sapphire-agent-server/src/agent.rs`, `crates/sapphire-agent-server/src/consult.rs`, `crates/sapphire-agent-server/src/tools/builtin_tools.rs`。

- [ ] **Step 1: IncomingMessage の owner と fixture を更新**

`pub attachment_owner: Option<AttachmentOwner>` を追加。Discord/Matrix は Task 4 の同じ owner を Some としてセット。heartbeat、consult fixture、Agent fixture、builtin_tools の Agent fixture は None。`rg -n 'IncomingMessage \{' crates/sapphire-agent-server/src` で全コンストラクタを確認。

- [ ] **Step 2: 起動と終了**

lib.rs の image cache と同じ起動層で AttachmentStore::default_dir(&workspace_dir) → open を spawn_blocking。失敗は warn して None。成功時、起動 sweep を終えてから AttachmentSaveTool を登録（ws_state の clone）。チャネルの new(...).with_attachment_store(store.clone()) に配線する。

掃除は既存 serve shutdown token の clone を使う tokio::select! loop。1時間 tick 時に spawn_blocking で sweep、エラーは warn。起動時 sweep は既に行うので interval の即時 tick は消費してから loop。shutdown で loop を抜け、serve終了時に JoinHandle を await（Runtime破棄に任せない）。store off なら掃除タスクなし。

- [ ] **Step 3: Agent の表示と実行**

Agent の tool_specs 取得 predicate は base visible_tool_predicate に次の例外だけ足す。incoming owner の有無はユーザー文字列からでなく構造体から取る。

```rust
let can_save = host_access_enabled && incoming.attachment_owner.is_some();
let base = crate::serve::visible_tool_predicate(
    host_access_enabled, false, false, false, false,
);
let specs = tools.specs_filtered(|name| {
    if name == "attachment_save" { can_save } else { base(name) }
}).await;
```

`partition_without_asking` の前に、当該 specs に無い call を Refusal::NotOffered で分離する。assistant の ToolUse は全callを残し、拒否にも必ず ToolResult を返す。permitted call の spawn 内で owner を明示 clone して scope_attachment_owner(owner, 既存の namespace/timer scoped future) する。取得・所有者 None の heartbeat から save が実行されないことをテストする。

- [ ] **Step 4: RED→GREEN 統合テスト**

Agent の既存 fake Provider/Channel fixture を拡張して以下を検査する:

- `channel_attachment_roundtrip_uses_original`: 受信PNG原本を insert、ownerと注記を持つ IncomingMessage → fake model が attachment_save → image_read → 第三provider呼出に実画像。出力ファイルは原本一致。
- `channel_with_host_access_off_cannot_save`: specs に2ツール無し。非提供名の直接要求を `attachment_save` と `image_read` の両方で実行し、各々 Permission/NotOffered の ToolResult、保存物無し・画像結果無しを assert。
- `foreign_owner_cannot_save_known_id`: ownerのroom/thread/platformをそれぞれ変更して拒否。
- `heartbeat_does_not_offer_attachment_save`: owner None。
- `restart_restores_attachment_lookup`: ストア再open、同owner/IDで保存。

Run: `cargo test -p sapphire-agent-server channel_attachment`、`cargo test -p sapphire-agent-server agent::tests`、`cargo test -p sapphire-agent-server` → exit 0。

Commit: `feat(agent): scope attachment saving to channel conversations`。

### Task 6: 履歴契約とドキュメント、最終検証

**Files:** Modify `crates/sapphire-agent-server/src/tools/attachment_tools.rs`（テスト）, `crates/sapphire-agent-server/src/agent.rs`（テスト）, `crates/sapphire-agent-server/src/serve/mod.rs`（テスト）, `docs/sapphire-agent-tools.md`, `crates/sapphire-agent-server/config.example.toml`。

- [ ] **Step 1: 履歴・provider 回帰テストを追加**

image_read の ToolOutput を実際に ChatMessage::tool_results_with_images に渡し、既存 image_cache の scrub_history_inplace と hydrate_history を通すテストを追加。画像キャッシュ on の終了後は ImageRef、後続provider入力は既存方針どおり marker、同じターンは画像、SessionStore append/reload で raw base64 がJSONLに出ないことを確認する。off 時も既存の永続化 marker が残ることを確認。provider の serialize テストは現行の Anthropic/OpenAI-compatible の既存テストを実行して画像 sibling 契約を守る（provider の実装自体は変えない）。

Run: `cargo test -p sapphire-agent-server image_read`、`cargo test -p sapphire-agent-server provider::`、`cargo test -p sapphire-agent-server image_cache`。

- [ ] **Step 2: 利用者文書**

ツール一覧に次を追記し、host_access の説明も2ツールを含めて整合させる。

```text
attachment_save(attachment_id, path): Discord / Matrix の同じ会話で受け取った添付原本を保存する。
image_read(path): agent 側の JPEG / PNG / GIF / WebP を1枚、画像として読む。
どちらも tools.host_access.enabled=true が必要。ACP では使えず、file_read は従来どおりテキスト用。
添付は画像以外も受信時に自動取得する。上限32 MiB/件・8件/投稿・64 MiB/投稿・一時ストア1 GiB。
受理した原本は7日保持し、期限内に attachment_save で保存する。保存済みファイルは自動削除しない。
容量不足・取得失敗・キャッシュ消失時は注記になる。画像の表示用縮小と保存する原本は別。
```

config.example.toml は新テーブルを作らず、既存 host_access のコメントに同じ制限を簡潔に追記。

- [ ] **Step 3: Windows を含む具体的な回帰テストと最終チェック**

POSIX 依存のない次のテストを `attachment_tools.rs` に追加し、Linux / macOS / Windows で実行する（cfg(unix) で囲まない）:

- `image_read_regular_file_uses_slash_path`: tempfile 配下の `nested/image.png` に有効な PNG を作り、実ツールで読み取る。画像1件・正しい MIME・元バイトサイズを assert し、text が `workspace::slash_path` で作った期待パスを含むことを確認する。
- `attachment_save_regular_file_uses_slash_path`: 同会話の原本を `nested/saved.png` に保存し、バイト一致と成功文の slash_path 表示を assert。その保存物を image_read で読み取り、画像結果を検証する。
- Windows 固有の `#[cfg(windows)]` テスト `windows_absolute_paths_read_and_save` は tempfile の絶対パス（ドライブ付き・ネイティブ区切り）を入力し、通常ファイルの読み取りと保存が成功すること、両結果の表示が slash_path と一致しバックスラッシュを含まないことを assert する。テスト対象パスは必ず tempfile で作り、固定ドライブを仮定しない。

ローカルでは `cargo test -p sapphire-agent-server regular_file_uses_slash_path` を実行。Windows CI の test-platforms でこれら2件と `windows_absolute_paths_read_and_save` が skip でなく実行・成功することをログで確認する。


```bash
cargo fmt --all -- --check
cargo test -p sapphire-agent-server
cargo test --workspace
cargo clippy --workspace --all-targets
git diff --check
```

Expected: 全コマンド exit 0、test failed=0。Windows CI の test-platforms が緑であることも統合前に確認。ローカル Linux 検証だけで Windows 実行済みとは報告しない。

- [ ] **Step 4: コミットと引き渡し**

Commit: `docs(tools): document attachment retention and image reading`。実装レポートに、実行コマンド・成功/失敗・残存制約（Matrix SDK全量バッファ）を記録し、spec/planからの変更があれば根拠とともに明記。#344 の実装に手を広げない。
