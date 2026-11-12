# 最終ブランチレビュー修正レポート（ラウンド1）

## 対応内容

### Important — 受け入れ条件8の provider 側回帰テスト

両 provider のテストに、非空の画像データを渡す `ChatMessage::tool_results_with_images` → provider 変換 → `serde_json::to_value` の検証を追加した。provider の実装は変更していない。

- Anthropic: シリアライズ結果全体を比較し、user メッセージ内に対応する `tool_use_id: call_read_image` の `tool_result`（結果本文を含む）と base64 画像ブロック（media type と非空のデータを含む）が保持されることを固定した。
- OpenAI-compatible: シリアライズ結果全体を配列として比較し、対応する `tool_call_id: call_read_image` の `role: tool` 結果の直後に、data URL 画像を含む `role: user` メッセージが保持されることを固定した。
- 画像は外部ファイルに依存しない非空の base64 テスト値 `aW1hZ2U=` を使用。画像デコードではなく wire format の保持を検証する。

### Minor — 不要になったモジュール全体の dead_code 抑止

`src/lib.rs` の `mod attachment_store;` に付いていた `#[allow(dead_code)] // Wired into the channel in Task 5.` を削除した。削除後の build で警告は再発しなかったため、追加の抑止や API 整理は不要だった。

## 検証結果

実行場所: `.worktrees/issue-395-396`（`feat/issue-395-396-attachments-image-read`）。すべてコミット前に実行。

| コマンド | 結果 |
| --- | --- |
| `cargo build -p sapphire-agent-server` | 成功、警告なし（21.47s） |
| `cargo test -p sapphire-agent-server provider::` | 56 passed; 0 failed; 0 ignored; 1189 filtered out（0.20s）。追加2テストも成功 |
| `cargo test -p sapphire-agent-server` | 1245 passed; 0 failed; 0 ignored（8.48s）。Doc-tests: 0 passed; 0 failed |
| `cargo fmt --all -- --check` | 成功 |
| `git diff --check` | 成功 |

build、provider テスト、全体テストのログに `warning:` / `error:` なし。
今回は既存動作の回帰テスト追加と属性削除のみで、TDD の RED は要求されていないため未実施。

## 変更ファイル

- `crates/sapphire-agent-server/src/provider/anthropic.rs`
- `crates/sapphire-agent-server/src/provider/openai_compatible.rs`
- `crates/sapphire-agent-server/src/lib.rs`
- `final-review-fix-report.md`

## セルフレビュー・懸念

- 差分は指定された2テスト、不要な属性の削除、このレポートに限定した。
- JSON 全体の比較により、結果本文・ID・画像データの欠落だけでなく OpenAI 側の順序も検証する。
- 懸念・未対応事項なし。

## PR #398 macOS CI 修正 — canonical workspace hash テストの移植性

### 実装・プラットフォームごとの判断

- `default_dir_hashes_canonical_lossless_workspace_bytes` は `#[cfg(target_os = "linux")]` に限定。無効 UTF-8 の `invalid-\xff` 名を作成する検証は Linux で維持し、EILSEQ で作成または canonicalize が失敗し得る macOS、および未確認の他 Unix には適用しない。
- cfg なしの `default_dir_hashes_canonical_workspace_path` を追加。有効 UTF-8 の `workspace-画像` と `.` 経由のパスを使い、canonical パスの実 OS 表現を SHA-256 に渡した期待値と比較する。Unix は生バイト、Windows は UTF-16 code unit の little-endian バイト列。canonical パスから直接解決した結果との同一性、別 root との非同一性も確認する。
- `canonicalize_failure_prevents_store_startup` を追加。存在しない workspace に対し起動時と同じ `default_dir(...).and_then(AttachmentStore::open)` が canonicalize の NotFound を伝播することを検証。production は変更なし。`lib.rs` の blocking 起動処理が `?` で失敗を返し、既存の warn → None に退避する境界を目視確認した（ログ自体の捕捉テストではない）。
- 残った Unix 条件を点検。store の symlink 作成・非追跡検証、および image_read の symlink escape/FIFO/device 拒否は macOS と Linux 双方で妥当なので `#[cfg(unix)]` を維持。`mkfifo` と `/dev/null` は両者で利用可能。`/proc/self/status` は macOS で存在しなくても拒否が期待結果であり、Linux 固有の読み取り成功を前提にしない。Unix の OS バイト取得と `O_NONBLOCK` にも変更不要。

### 検証（コミット前、Linux 実行）

| コマンド | 結果 |
| --- | --- |
| `cargo test -p sapphire-agent-server attachment_store` | exit 0; 17 passed; 0 failed; 1230 filtered out |
| `cargo test -p sapphire-agent-server` | exit 0; 1247 passed; 0 failed; Doc-tests 0 passed / 0 failed |
| `cargo fmt --all -- --check` | exit 0 |
| `git diff --check` | exit 0 |

両テストログに warning/error なし。今回の回帰テスト修正では TDD RED は要求されておらず未実施。元の macOS EILSEQ は CI 報告に基づき、この Linux 環境では再現していない。

### 変更ファイル・セルフレビュー

- `crates/sapphire-agent-server/src/attachment_store.rs`（テストのみ）
- `final-review-fix-report.md`（本セクション）

production の挙動・API は不変。Linux の非 UTF-8 回帰検証を失わず、共通検証を独立して残した。macOS/Windows の実行はこの環境では未実施であり、CI での確認が必要。その他の懸念なし。
