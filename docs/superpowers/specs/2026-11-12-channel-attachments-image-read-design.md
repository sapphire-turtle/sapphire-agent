# チャネル添付の保存とローカル画像の読み取り（#395・#396）

- **状態**: 設計
- **日付**: 2026-11-12
- **対象**: sapphire-agent-server。Discord / Matrix の受信、添付一時ストア、`attachment_save`、`image_read`。
- **前提**: `bebc6db`（#397 マージ後）。画像の 5 MiB 制限、32 MiB 取得上限、縮小は `channel::prepare_image` を共用する。

## 背景と目的

現在は画像だけを受信し、ターン終了後の画像キャッシュには元のファイル名が残らない。非画像は Discord で注記になるだけで、Matrix の `m.file` / `m.audio` / `m.video` は無視される。また `file_read` はテキスト用で、保存済みの画像をモデルに見せられない。

添付を原本のまま明示保存でき、その画像をパスから再び見られるようにする。添付の自動取得と、任意パスへの書き込みの権限は別に扱う。既存の画像インライン入力、履歴の `ImageRef`、`recall_image` は維持する。

## 決定事項

### フミトの決定（2026-11-12）

1. **画像以外も受信時に自動取得し、上限付き一時キャッシュに置く。** 保存を頼まれる時点では Discord の署名付き URL が失効し得るため。取得元 URL を保存要求時に取り直す設計にはしない。
2. **未保存添付の保持期間は7日。** 後日の保存要求に対応しつつ長期保管は明示保存に分ける。明示保存したファイルは自動掃除しない。

### 技術判断と根拠

- 原本専用の `AttachmentStore` を画像キャッシュと分離する。縮小した JPEG を元 PNG として書き出さないため。原本も7日で削除対象となり、保存成功で保持延長はしない。
- `image_read(path)` を独立ツールにする。`file_read` のテキストのページングと ACP クライアント fs の契約を変えないため。
- 2ツールとも今回は **agent 側のみ、ACP では非表示かつ実行時拒否**。ACP v1 の `fs/read_text_file` はバイナリ用でなく、terminal に base64 を流す新経路まで増やす必要はない。ACP から agent ホストに黙ってフォールバックしない。
- 両ツールに `[tools.host_access].enabled` を適用する。workspace 内も既存 `file_read` / `file_write` と同じ opt-in に揃える。自動キャッシュは任意ホストアクセスではないので、この switch が off でも動く。
- 添付 ID は UUID v7（標準文字列表現）。所有者は platform・room・thread の組。別の会話で ID を知っても保存できない。認証主体は既存 room の参加者・allowlist とし、同じ room/thread 内の sender 分離はしない。
- 添付保存はチャネルターンだけに提供する。共有 ToolSet に登録しても、task-local に所有者がない呼び出しは拒否する。RPC・heartbeat・subagent に会話の添付権限を暗黙継承させない。
- 32 MiB/ファイル、8件/メッセージ、合計64 MiB/メッセージ、ストア全体1 GiB。7日未満のエントリを容量都合で追い出さず、新規受信を拒否する。保持期間の約束とディスク枯渇対策を両立するため。初版は定数とし設定項目は増やさない。

## 1. 受信と一時保存

### 1.1 フロー

既存の room/user/bot フィルターの **後** に取得する。Discord の全添付、Matrix の Image / File / Audio / Video が対象。MIME 不明でも非画像ファイルとして取得する。caption と reply/thread の意味は既存どおり。

1. メタデータの件数・申告サイズで事前選別。
2. 取得（1件60秒以内）、実バイト数で再検査。
3. 原本をストアに確定して ID を発行。
4. `sniff_image_mime` が対応画像なら別コピーを `prepare_image` に渡し、従来の `IncomingMessage.attachments` へ入れる。非画像は画像パートに入れない。
5. 全添付についてモデルに本文注記を渡す。ストア失敗でも既存の画像入力は可能な限り維持し、保存不能とインライン処理失敗は別々に伝える。

`Attachment` は画像専用の現行形のまま。`IncomingMessage` に `attachment_owner: Option<AttachmentOwner>` を追加する。Discord / Matrix だけ Some、heartbeat と既存テスト fixture は None とする。構造化添付一覧はストアにあり、本文には説明だけを残す。

成功注記の例（値は JSON エスケープした文字列として表示し、改行・制御文字をそのまま出さない）:

```text
[Attachment id="019..." filename="report.pdf" media_type="application/pdf" bytes=1234 expires_at_ms=179...; cached original, not viewed inline. Use attachment_save to save it.]
```

画像なら `cached original, image shown inline`。縮小時は「inline image may be resized; saved bytes are original」を追加する。失敗は `dropped_attachment_note` を利用し、保存可能な ID は発行しない。取得成功・保存成功・インライン失敗を混同しない。添付しかない投稿も注記によりターンになる。

### 1.2 ストアと掃除

新規 `src/attachment_store.rs`。標準 cache dir の `sapphire-agent/attachments/<workspace-key>/`。workspace-key は canonical workspace root の OS 文字列表現を lossless なコード単位列でハッシュする（Unix は OsStrExt::as_bytes、Windows は encode_wide の各 u16 の little-endian bytes）。他の workspace の容量・ID を共有しない。`image_cache.enabled` には従属しない。

各 ID に `<id>.bin` と `<id>.json`。JSON は version=1、id、owner、message_id、元 filename、申告 MIME（なければ application/octet-stream）、実サイズ、received_at_ms、expires_at_ms を持つ。URL・認証情報・Matrix の暗号鍵は保持しない。元ファイル名はディスクのパス組み立てに一切使わない。未完了は `.tmp` で書き、bin → JSON の順で rename。JSON が commit marker。成功前に両方が揃う必要がある。

プロセス内 Mutex で insert / sweep / get を直列化し、容量チェックと確定を一体にする。I/O は `spawn_blocking` で行う。キャッシュディレクトリは一つの serve プロセスが所有する前提（同一 workspace の複数 serve はサポートしない）。キャッシュ下の symlink は読まない。ID は UUID の再解析・正規形一致を確認してからパス化する。

起動時と1時間ごとに sweep。`now >= expires_at_ms` は期限切れで、get 時にも拒否する。読み取り・保存で期限を更新しない。壊れた JSON、孤立 bin / tmp は起動時に削除し warn。未来の version は読まず、そのエントリは残して容量に算入する。掃除はストア直下の既知の生成ファイルだけに限定し、workspace や保存先を走査しない。

1 GiB はストア内の bin・json・tmp の合計。insert は原本 + メタデータの追加量込みで検査。期限切れを掃除しても足りなければ拒否。上限を超える既存ストアは削除で縮めず、新規を拒否する。起動失敗は warn、ストアなしでチャネルを継続し、`attachment_save` は登録しない。画像入力と `image_read` は独立して動く。

### 1.3 取得上限の意味

件数は元の順序の先頭8件まで。申告で落としたものも枠を消費する。サイズ合計は成功して取得した原本の実サイズ。残量を超えるものはストアに入れず、後続の小さい添付は試行できる。

Discord は reqwest の chunk 読みで残量+1 byte を超えたら中断し、Content-Length と status も確認する。イベントに載る CDN URL のみ使用し、ツール入力から URL を受けない。Matrix は SDK の認証・E2EE 復号済み `get_media_content` を使用し、事前サイズと取得後サイズを検査、SDK キャッシュは false とする。**Matrix SDK は Vec を返すため転送中の厳密なメモリ上限ではない**。保存上限と60秒 timeout は保証するが、この API の内部バッファを新しい HTTP 実装で置き換えない。

## 2. `attachment_save(attachment_id, path)`

- `ToolKind::Edit`。path は絶対・`~/`・workspace 相対を許可。原本をそのまま保存し、既存ファイルは `file_write` と同じく上書きする。
- task-local の owner とストア owner が一致し、期限内のエントリだけ取得。別 owner、存在しない ID、期限切れには共通の `attachment unavailable in this conversation or expired` を返し、メタデータを漏らさない。
- 保存前に既存 `refuse_if_sensitive` を共有し、`WorkspaceState::write_bytes` を使う。framework の path / API-key 保護、index と sync の更新を通す。ファイル名・MIME を信用して実行しない。
- 成功結果は保存先（`workspace::slash_path`）・原本のバイト数・ID。失敗時に成功を返さない。コピー後のファイルは7日掃除と無関係。
- モデルに見えるには host_access on かつチャネル owner が必要。実行側も host_access gate に登録し、ACP scope があれば必ず拒否する。`Agent` の offered-tool 検査にも従わせ、非提供名を直接呼んでも実行しない。
- owner は Agent の当該受信ターンの各 tool 実行 future に bind する。tokio::spawn には明示的に再bind。subagent の再帰実行は owner を None に scope し、継承を遮断する。新しい名前を許可リストに足すだけで権限を捏造できない。

## 3. `image_read(path)`

- `ToolKind::Read`。host_access on の非 ACP でだけ提供。ACP scope の実行は明示拒否する。
- `WorkspaceState::resolve_path` を通し、既存 file_read と同じ tilde 展開・外部パス許可に従う。解決後の `/dev`・`/proc` 以下を拒否し、通常ファイルだけを開く。ディレクトリ、FIFO、socket は拒否してブロックしない。
- metadata と読み取り途中の両方で32 MiBを検査する。`File::take(MAX_DOWNLOAD_BYTES as u64 + 1)` で有界読み取りし、増大中のファイルも拒否する。
- 拡張子・申告 MIME に頼らず `prepare_image` を `spawn_blocking` で呼ぶ。1回1画像。5 MiB超なら既存の1568px / JPEG縮小を使う。保存ファイル自体は変更しない。
- `execute_full` が `ToolOutput { text, images: vec![(mime, base64)] }` を返す。text は表示パス、元サイズ、返す MIME と縮小有無。通常 `execute` も同じ検証を実行し text を返す（成功したふりの固定文にはしない）。
- HEIC / SVG / その他は理由つきエラー。SVG は `file_read` でテキストとして読むことを案内する。URL は取得しない。
- 既存 `tool_results_with_images` → provider → ターン終了時 scrub を使う。JSONL と後続ターンに raw base64 を増やす新形式は導入しない。image_cache off 時の既存劣化挙動も変えない。

## 4. 受け入れ条件

1. Discord の PDF と Matrix の File / Audio / Video は、caption がなくても注記と ID が届き、期限内に原本を保存できる。
2. 5 MiB 超の画像の inline 表示は従来どおり縮小されるが、保存物は入力原本と byte-for-byte 一致する。
3. 再起動後も7日以内の同じ owner の ID で保存でき、期限ちょうどで拒否される。読んでも期限を延長しない。
4. 件数・実サイズ・合計・総容量・timeout の失敗は注記になり、他の添付や本文を消さない。
5. 他 room/thread/platform、RPC、subagent、ACP、host_access off から添付保存を迂回できない。
6. image_read の PNG/JPEG/GIF/WebP は実 MIME の画像結果になる。32 MiB超、非画像、特殊ファイルはエラー。テキスト file_read の既存挙動は不変。
7. 保存先の sensitive path / API-key 保護が保たれ、workspace 内のバイナリ保存は index/sync の既存経路を通る。
8. Anthropic / OpenAI-compatible の画像ツール結果と、ターン後の ImageRef / JSONL の既存契約をテストで固定する。
9. Windows のパス表示、通常ファイル処理、保存を検証する。Unix 固有 symlink/FIFO テストだけ cfg(unix)。

## 5. リスクと範囲外

- 7日保持は「空きがあるときに受理した添付」の期間。容量拒否、キャッシュ消失、OS による cache 清掃、I/O 障害は保存不能として伝える。永続バックアップではない。
- Matrix SDK の内部バッファと既存 prepare_image の decode メモリは別の制約。信頼された allowlist のメッセージを扱う前提は変えない。ファイル型確認と open の間の敵対的なホスト側差し替えへの完全な sandbox は今回追加しない。
- #344 は範囲外。将来も ToolOutput.images と tool_results_with_images は共用できる。
- ACP のバイナリ転送、PDF解析、音声認識、動画フレーム抽出、保存時の自動画像読取、RPC/IPC の添付形式変更、既存 image_cache の掃除は含めない。
- **未決定事項なし。** 技術判断への反論は仕様レビューで扱い、プロダクト決定2点と区別する。
