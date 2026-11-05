# デフォルトキャラクターの素材パック（素材リポジトリと、agent のインストーラー）

- **対象**: 新設リポジトリ `sapphire-turtle/sapphire-agent-default-avatar`（仮称）、
  `crates/sapphire-agent-server/`（新設モジュール `default_avatar`）
- **参照**: `crates/sapphire-agent-core/src/lib.rs` の `AvatarConfig` と `AVATAR_MAX_FILE`、
  `crates/sapphire-agent-server/src/avatar_store.rs`（パスの検証と、`avatar/` の外での一時置き）、
  `crates/sapphire-agent-server/src/voice/providers/sherpa_download.rs`（取得と展開の前例）、
  [デスクトップマスコットの仕様](2026-10-28-desktop-companion-design.md)
- **関連 issue**: #340（デフォルトキャラクターと初回起動の流れ）のやること 1。
  ワークスペースの既定の場所は sapphire-turtle/sapphire-framework#398（framework 0.26.0）
- **状態**: 設計

## なぜ

デスクトップマスコットは、ユーザーが VRM モデルや立ち絵を用意しないとキャラクターが出ない。
新規ワークスペースにデフォルトのオリジナルキャラクターを入れておき、すぐに使えるようにする（#340）。

#340 で決めた前提:

- 素材はソースコードと別のリポジトリに置き、GitHub Releases で配布する。
- 新規ワークスペースの初期化時に**一度だけ**取得し、`<workspace>/avatar/` に展開する。
  取り込んだあとはユーザーが置いた素材と同じ扱いで、削除も差し替えも自由。
- 自動アップグレードはしない。新しいパックが効くのは、その後に作ったワークスペースだけ。
- オフライン用の代替は埋め込まない（アプリはオンラインが前提）。
- パックは見た目と、`profile.md`（名前・口調などの**案**）だけ。人格はブートストラップで
  ユーザーが決め、`IDENTITY.md` / `SOUL.md` に書く。
- ワークスペースの同期は 1 GiB まで。

## 範囲

この仕様が扱うのは次の 2 つ。

1. 素材リポジトリ: 構成、ライセンス、検証、リリースの形式（§1・§2）
2. agent 側の、パックを取得して `avatar/` に入れる部品（§3）

扱わないもの（#340 の別のやること）:

- どの `init`（agent 単独の `sapphire-agent init` か、サーバー経由の framework の
  `workspace init` か）がこの部品を呼ぶか、`init` の既定の場所の追従（やること 2）
- `BOOTSTRAP.md` の改訂（やること 4）
- GUI の初回起動画面と、取得をやり直す入口（IPC メソッドや設定ページのボタン。やること 5）
- 素材そのものの制作

## 決定事項

| 項目 | 決定 |
|---|---|
| ライセンス | CC0 1.0。VRM の `meta` も CC0 で、利用・改変・再配布・商用をすべて許可 |
| 公開リポジトリに置くもの | 配布物（`avatar/` 以下）、LICENSE、README、検証用スクリプト、CI だけ。制作用の中間ファイルや制作記録は置かない |
| README | 見本の画像、ファイル一覧、ライセンス。制作方法には触れない |
| 最初のリリース | 2D（立ち絵）と 3D（VRM + VRMA）を両方そろえてから出す |
| 配布の形 | リリースごとに無圧縮の tar 1 つ + sha256。agent は URL と sha256 を埋め込む |
| バージョン | `v1`、`v2`… の整数 |
| パック全体の上限 | 100 MB（VRM の実寸を見て見直す） |

### 配布の形を選んだ理由

- **tar 1 つ + sha256 の固定**（採用）: 取得 1 回、検証 1 回。`sherpa_download.rs` の
  一時ディレクトリに展開して rename する流れをそのまま使える。途中で失敗すると最初から
  やり直しになるが、数十 MB なので許容する。
- ファイルごとに manifest で取得: 部分的な取り直しや差分更新ができるが、自動アップグレードを
  しないので使い道がない。
- GitHub が自動で作るソースアーカイブ（`/archive/refs/tags/…`）: バイト単位で同じである
  保証がなく、sha256 が変わった前例があるので固定に向かない。

無圧縮にするのは、PNG・WebP・VRM がすでに圧縮済みで、もう一度圧縮してもほとんど縮まないため。
agent はすでに使っている `tar` クレートだけで読める。

## §1 素材リポジトリ

```
sapphire-agent-default-avatar/
  avatar/                  ← そのまま <workspace>/avatar/ に展開される
    avatar.toml            ← 既存の形式（[vrm] と [sprite]）
    profile.md             ← 名前・口調・一人称・雰囲気の案
    sprite/<pose>.webp
    sprite/<pose>-<expression>.webp
    vrm/model.vrm
    vrm/idle.vrma
    vrm/<action>.vrma
  LICENSE                  ← CC0 1.0
  README.md
  scripts/check            ← §2 の検証
  scripts/strip            ← §2 の掃除
  .github/workflows/       ← CI とリリース
```

- `avatar.toml` は agent の `AvatarConfig` の形式そのまま。新しい形式は作らない。
  表情は VRM 1.0 の感情プリセット（neutral と `SPRITE_EXPRESSIONS` の 5 種）を 2D と 3D で
  そろえる。
- 立ち絵は PNG か WebP（GUI はどちらも読める）。サイズの小さい WebP を基本にする。
- **LFS は使わない**。配布物は 1 ファイルあたり数十 MB までで、GitHub の上限
  （50 MB で警告、100 MB で拒否）に収まる。更新はまれなので履歴の増加は許容する。
- `profile.md` は案であって設定ではない。agent はこのファイルを読み込みに使わず、
  ブートストラップが提案の材料として読むだけ（#340 のやること 4）。

### リリース

- tag `vN` を push すると、CI が `scripts/check` を通したうえで `avatar/` から
  `default-avatar-vN.tar` を作り、`default-avatar-vN.tar.sha256` と一緒にリリースに添付する。
- **tar は CI でだけ作り、手元では作らない**。再現できるように、エントリーを名前順に並べ、
  mtime を 0、所有者を数値の `0:0` に固定する（GNU tar の `--sort=name --mtime=@0
  --owner=0 --group=0 --numeric-owner --format=ustar`）。所有者を固定するのは、
  tar がユーザー名とグループ名を記録し、作った人の環境の名前がパックに残るのを防ぐためでもある。
- tar の最上位は `avatar/`。agent は展開時にこれを取り除く。

## §2 検証（`scripts/check`）

PR ごとと、リリースの前に CI で走らせる。手元でも同じに動くよう、**Python の標準ライブラリだけ**で
書く（TOML は `tomllib`、PNG・WebP・glTF バイナリは自前で読む）。exiftool などの外部ツールは使わない。

### メタデータ

編集・書き出しツールは、作成ツール名・設定・作者名などを画像やモデルに埋め込む。
公開する素材にはこれを残さない。危ないものを列挙するのではなく、**安全と分かっているものだけを
許し、それ以外があれば失敗する**。将来のツールが新しい種類を足しても素通りしないようにするため。

- **PNG**: 許可するチャンクは `IHDR` `PLTE` `IDAT` `IEND` `tRNS` `sRGB` `gAMA` `pHYs` だけ。
  `tEXt` / `zTXt` / `iTXt`、`eXIf`、C2PA の `caBX`、`iCCP`（プロファイル名にツール名が入りうる）は
  失敗にする。
- **WebP**: 許可するチャンクは `VP8 ` `VP8L` `VP8X` `ALPH` `ANIM` `ANMF` だけ。`EXIF`、`XMP `、
  `ICCP` は失敗にする。
- **VRM / VRMA（glTF バイナリ）**:
  - **モデルに埋め込まれた画像**（テクスチャ）も、PNG・WebP と同じ規則で調べる。
  - 画像・テクスチャ・マテリアルなどの名前は、ASCII の英数字と `_` `-` だけ、64 文字まで。
    書き出し元のファイル名がそのまま残るのを防ぐ。
  - VRM の `meta`: 作者はプロジェクト名 1 つ（`sapphire-agent`）、ライセンスは CC0、
    許可の項目はすべて許可。
  - `asset.generator` は、決めた値（`sapphire-agent-default-avatar`）と一致すること。

### パックの中身

- `avatar.toml` が読め、既存の規則を満たす（`default_pose` が `poses` にある、表情の名前が
  `SPRITE_EXPRESSIONS` のどれか、など）。意味の正しさの基準は agent の
  `AvatarConfig::validate` で、agent も展開後に検証する（§3）ので、ここで完全に再現しなくてよい。
- `avatar.toml` が参照するファイルがすべてあり、`avatar/` の下に**参照されていないファイルがない**
  （`profile.md` を除く）。
- 立ち絵はすべて同じキャンバスサイズ（2D の仕様が前提にしている）。
- パック全体が 100 MB 以下、1 ファイルが `AVATAR_MAX_FILE`（256 MiB）以下。

### 掃除（`scripts/strip`）

- PNG・WebP から、許可したチャンク以外を取り除いて書き直す。
- VRM・VRMA の `asset.generator` を決めた値に書き換える（JSON チャンクだけを書き直し、
  バイナリチャンクには触れない）。編集ツールの多くはこの値を変えられないため。
- VRM の `meta`、各種の名前、モデルに埋め込むテクスチャは、編集ツールの側で直す
  （テクスチャは取り込む前に PNG・WebP として掃除しておく）。直し忘れは `scripts/check` が捕まえる。

## §3 agent のインストーラー

`crates/sapphire-agent-server/src/default_avatar.rs` を新設する。

```rust
/// The default character pack new workspaces get, pinned.
pub(crate) const PACK: Pack = Pack {
    version: 1,
    url: "https://github.com/sapphire-turtle/sapphire-agent-default-avatar/releases/download/v1/default-avatar-v1.tar",
    sha256: "…", // the v1 release's `.sha256`, filled in when v1 is released
};

pub(crate) enum Outcome {
    Installed,
    /// `avatar/avatar.toml` was already there; nothing was touched.
    AlreadyPresent,
}

pub(crate) fn install(
    workspace_root: &Path,
    progress: impl FnMut(u64 /* received */, Option<u64> /* total */),
) -> anyhow::Result<Outcome>;
```

新しいパックに切り替えるのは、`PACK` を書き換える PR。

### 流れ

1. `<workspace>/avatar/avatar.toml` があれば、何もせず `AlreadyPresent` を返す
   （テンプレートの展開と同じく、上書きしない）。
2. キャッシュディレクトリの一時ファイルへストリーミングで取得し、同時に sha256 を計算する
   （`sherpa_download.rs` のように全体をメモリに載せない）。`progress` に受信量と全体の大きさ
   （`Content-Length` があれば）を渡す。sha256 が違えば失敗。
3. キャッシュディレクトリの作業用ディレクトリに展開する（§3.1 の規則）。ワークスペースの
   外に置くのは、展開途中の状態を同期に見せないため。
4. 作業用ディレクトリで、`avatar.toml` が `AvatarConfig` として読めて `validate` を通り、
   参照するファイルがすべてあることを確かめる。
5. `<workspace>/avatar/` に移す。キャッシュが別のファイルシステムにあって rename できなければ、
   `avatar_store` と同じくコピーに切り替える。`avatar/` が空のディレクトリとしてすでにあれば、
   その中に移す。
6. 一時ファイルと作業用ディレクトリを消す。

### 3.1 展開の安全

tar は外から来るので厳しく見る。

- 受け付けるのは通常のファイルとディレクトリだけ。シンボリックリンク、ハードリンク、
  デバイスファイルなどがあれば失敗。
- パスは `avatar/` で始まり、それを取り除いた残りの各要素が `avatar_store::validate_path` と
  同じ規則（`..` や `\`、`:` などを含まない）を満たすこと。
- 1 ファイルは `AVATAR_MAX_FILE` まで、全体は 100 MB まで。超えた時点で中止する。

### 失敗したとき

インストールに失敗しても、ワークスペースの初期化は失敗にしない。警告を出し、マスコットは
今の仕様どおりの仮表示（シルエット）になる。`install` は冪等なので、やり直しはもう一度呼ぶだけ。
やり直しの入口（IPC メソッドや設定ページ）は #340 のやること 2・5 で決める。

### 依存

`tar` は今は音声の feature の中で optional。小さいので通常の依存にする。HTTP は既存の
`reqwest`（blocking）、ハッシュは既存の `sha2` を使う。

### テスト（ネットワークなし）

取得とそれ以外を分け、展開と検証は任意の reader を受け取る関数にして、テストの中で作った tar で
確かめる。

- 正しいパックが入り、`Installed` を返す
- sha256 が違うと失敗し、`avatar/` に何も残らない
- `avatar.toml` がすでにあると `AlreadyPresent` で、何も変わらない
- `../` を含むエントリー、`avatar/` の外のエントリー、シンボリックリンクのエントリーで失敗する
- `avatar.toml` が参照するファイルが欠けていると失敗する
- 上限を超えるファイルで失敗する
- 失敗のあとに `avatar/` が半端な状態で残らない

## 未決のこと

- 素材リポジトリの名前（仮称 `sapphire-agent-default-avatar`）
- 最初のポーズとアクションのセット（素材の制作と合わせて決める）
- パック全体の上限 100 MB の妥当性（VRM の実寸を見て見直す）
