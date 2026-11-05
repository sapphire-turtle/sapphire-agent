# デフォルトキャラクターの素材（素材リポジトリと、agent のインストーラー）

- **対象**: 新設リポジトリ `sapphire-turtle/sapphire-agent-avatars`（仮称）、
  `crates/sapphire-agent-server/`（新設モジュール `default_avatar`）
- **参照**: `crates/sapphire-agent-core/src/lib.rs` の `AvatarConfig` と `AVATAR_MAX_FILE`、
  `crates/sapphire-agent-server/src/avatar_store.rs`（パスの検証と、`avatar/` の外での一時置き）、
  `crates/sapphire-agent-server/src/voice/providers/sherpa_download.rs`（取得と展開の前例）、
  [デスクトップマスコットの仕様](2026-10-28-desktop-companion-design.md)
- **関連 issue**: #340（デフォルトキャラクターと初回起動の流れ）のやること 1。
  ワークスペースの既定の場所は sapphire-turtle/sapphire-framework#398（framework 0.26.0）。
  1 つのワークスペースに複数のアバターを置いて切り替える仕組みは別の issue で扱う
- **状態**: 設計

## なぜ

デスクトップマスコットは、ユーザーが VRM モデルや立ち絵を用意しないとキャラクターが出ない。
新規ワークスペースにデフォルトのオリジナルキャラクターを入れておき、すぐに使えるようにする（#340）。

#340 で決めた前提:

- 素材はソースコードと別のリポジトリに置き、GitHub Releases で配布する。
- 新規ワークスペースの初期化時に**一度だけ**取得し、`<workspace>/avatar/` に展開する。
  取り込んだあとはユーザーが置いた素材と同じ扱いで、削除も差し替えも自由。
- 自動アップグレードはしない。新しい版が効くのは、その後に取得したものだけ。
- オフライン用の代替は埋め込まない（アプリはオンラインが前提）。
- 素材は見た目と、`profile.md`（名前・口調などの**案**）だけ。人格はブートストラップで
  ユーザーが決め、`IDENTITY.md` / `SOUL.md` に書く。
- ワークスペースの同期は 1 GiB まで。

その後に決めたこと:

- デフォルトキャラクターは 1 人。ただし**通常の等身とデフォルメの等身**の 2 種類を、
  2D・3D とも用意する。
- 衣装は少しずつ増やす（季節の衣装など）。初回に入るのは 1 つだけで、ほかはあとから
  追加で取得する（無料の「ショップ」。#340 のやること 5 以降）。

## 範囲

この仕様が扱うのは次の 2 つ。

1. 素材リポジトリ: 構成、ライセンス、検証、リリースとカタログの形式（§1・§2）
2. agent 側の、アバターを 1 つ取得して `avatar/` に入れる部品（§3）

扱わないもの:

- どの `init`（agent 単独の `sapphire-agent init` か、サーバー経由の framework の
  `workspace init` か）がこの部品を呼ぶか、`init` の既定の場所の追従（#340 のやること 2）
- `BOOTSTRAP.md` の改訂（やること 4）
- GUI の初回起動画面と、取得をやり直す入口（IPC メソッドや設定ページのボタン。やること 5）
- ショップの画面と、カタログを読んで追加のアバターを入れる流れ
- 1 つのワークスペースに複数のアバターを置き、選んで切り替える仕組み（別の issue）。
  それまでは、入れる先は今の `avatar/` 1 つ
- 素材そのものの制作

## 用語

- **キャラクター**: デフォルトのオリジナルキャラクター（1 人）。`profile.md` はキャラクター単位。
- **アバター**: 配布の単位。**衣装 1 つ × 等身 1 つ**で、2D と 3D を両方含む。
  ID は `<衣装>-<等身>`（例: `default-regular`、`default-chibi`、`santa-chibi`）。
  等身は `regular`（通常）と `chibi`（デフォルメ）。

## 決定事項

| 項目 | 決定 |
|---|---|
| リポジトリ | キャラクター 1 人につき 1 つ。アバターはディレクトリで分ける（アバターごとのリポジトリにはしない） |
| ライセンス | CC0 1.0。VRM の `meta` も CC0 で、利用・改変・再配布・商用をすべて許可 |
| 公開リポジトリに置くもの | 配布物、LICENSE、README、検証用スクリプト、CI だけ。制作用の中間ファイルや制作記録は置かない |
| README | 見本の画像、アバターの一覧、ライセンス。制作方法には触れない |
| 最初のリリース | 最初のアバターの 2D（立ち絵）と 3D（VRM + VRMA）を両方そろえてから出す |
| 配布の形 | アバターごと・版ごとに無圧縮の tar 1 つ + sha256 |
| バージョン | アバターごとに `v1`、`v2`… の整数。tag は `<id>-vN` |
| 一覧 | CI が作る `catalog.json` を、決まった URL に置く |
| sha256 の信頼 | 初回に入るアバターは agent に埋め込んだ値で確かめる。追加のアバターはカタログの値で確かめる |
| アバター 1 つの上限 | 100 MB（VRM の実寸を見て見直す） |

### リポジトリを分けない理由

- ショップには「何があるか」の一覧が要る。1 つのリポジトリなら `catalog.json` 1 つで済む。
  アバターごとのリポジトリだと、GitHub API でリポジトリを列挙することになり、
  レート制限や、どれが公開してよいものかの区別が問題になる。
- `profile.md`、LICENSE、`scripts/check`、CI はキャラクターで共通。リポジトリを分けると
  衣装を足すたびに複製になり、ずれていく。
- リポジトリの大きさが効くのは clone する作者だけ。ユーザーはリリースの tar だけを取得する。
  GitHub はリポジトリを 1 GB 以下に保つよう勧めている（5 GB で強く求める）。衣装が増えて
  近づいたら、バイナリを git から外してリリースの添付だけにする（§1 の「大きくなったら」）。

キャラクターが増えたら、キャラクターごとにリポジトリを分ける。

### 配布の形を選んだ理由

- **アバターごとの tar 1 つ + sha256**（採用）: 取得 1 回、検証 1 回。`sherpa_download.rs` の
  一時ディレクトリに展開して rename する流れをそのまま使える。途中で失敗すると最初から
  やり直しになるが、数十 MB なので許容する。
- ファイルごとに manifest で取得: 部分的な取り直しや差分更新ができるが、自動アップグレードを
  しないので使い道がない。
- GitHub が自動で作るソースアーカイブ（`/archive/refs/tags/…`）: バイト単位で同じである
  保証がなく、sha256 が変わった前例があるので固定に向かない。また、リポジトリ全体が入る。

無圧縮にするのは、PNG・WebP・VRM がすでに圧縮済みで、もう一度圧縮してもほとんど縮まないため。
agent はすでに使っている `tar` クレートだけで読める。

### sha256 の信頼を 2 段にする理由

初回に入るアバターは agent の版と一緒に決まるので、URL と sha256 を agent に埋め込める。
GitHub のリリースが差し替えられても、初回の取得は検出できる。

追加のアバターは agent のリリースよりあとに出るので、埋め込めない。カタログの sha256 で
確かめる。これは取得の途中での破損は検出するが、信頼の根は GitHub と HTTPS になる。
カタログへの署名（minisign など）は、必要になったら足す。

## §1 素材リポジトリ

```
sapphire-agent-avatars/
  character/
    profile.md             ← 名前・口調・一人称・雰囲気の案（キャラクター単位）
  avatars/
    <id>/                  ← 1 つのアバター。tar では avatar/ になる
      avatar.toml          ← 既存の形式（[vrm] と [sprite]）
      sprite/<pose>.webp
      sprite/<pose>-<expression>.webp
      vrm/model.vrm
      vrm/idle.vrma
      vrm/<action>.vrma
  thumbnails/<id>.webp     ← ショップに出す見本（tar には入れない）
  catalog.toml             ← アバターの一覧（手で書く。CI が catalog.json にする）
  LICENSE                  ← CC0 1.0
  README.md
  scripts/check            ← §2 の検証
  scripts/strip            ← §2 の掃除
  scripts/catalog          ← catalog.json を作る
  .github/workflows/       ← CI とリリース
```

- `<id>` は ASCII の小文字・数字・`-` だけ、`<衣装>-<等身>` の形。等身は `regular` か `chibi`。
- `avatar.toml` は agent の `AvatarConfig` の形式そのまま。新しい形式は作らない。
  表情は VRM 1.0 の感情プリセット（neutral と `SPRITE_EXPRESSIONS` の 5 種）を 2D と 3D で
  そろえる。
- 立ち絵は PNG か WebP（GUI はどちらも読める）。サイズの小さい WebP を基本にする。
- **LFS は使わない**。配布物は 1 ファイルあたり数十 MB までで、GitHub の上限
  （50 MB で警告、100 MB で拒否）に収まる。
- `profile.md` は案であって設定ではない。agent はこのファイルを読み込みに使わず、
  ブートストラップが提案の材料として読むだけ（#340 のやること 4）。リポジトリには 1 つだけ置き、
  CI が各アバターの tar に同じものを入れる。どのアバターから始めても手元にあるようにするため。

### `catalog.toml`

```toml
[[avatar]]
id = "default-chibi"
outfit = "default"
proportion = "chibi"     # "regular" | "chibi"
title = { en = "…", ja = "…" }
```

並び順はショップに出す順。衣装の名前などの表示用の項目はここに置き、`avatars/<id>/` には
`avatar.toml` 以外の設定を置かない。

### リリース

- tag `<id>-vN` を push すると、CI が次を行う。
  1. `avatars/<id>/` と `catalog.toml` の項目があることを確かめ、`scripts/check` を通す。
  2. `avatars/<id>/` を `avatar/` として、`character/profile.md` を `avatar/profile.md` として
     tar にまとめ、`<id>-vN.tar` と `<id>-vN.tar.sha256` をリリースに添付する。
  3. `scripts/catalog` で `catalog.json` を作り直し、`catalog` という名前の固定のリリースに
     添付し直す（§1.1）。
- **tar は CI でだけ作り、手元では作らない**。再現できるように、エントリーを名前順に並べ、
  mtime を 0、所有者を数値の `0:0` に固定する（GNU tar の `--sort=name --mtime=@0
  --owner=0 --group=0 --numeric-owner --format=ustar`）。所有者を固定するのは、
  tar がユーザー名とグループ名を記録し、作った人の環境の名前が残るのを防ぐためでもある。
- tar の最上位は `avatar/`。agent は展開時にこれを取り除く。
- 一度出した `<id>-vN` のリリースは差し替えない（agent が sha256 を埋め込んでいる）。
  直すときは `vN+1` を出す。

### 1.1 `catalog.json`

URL は固定で、`https://github.com/sapphire-turtle/sapphire-agent-avatars/releases/download/catalog/catalog.json`。

```json
{
  "schema": 1,
  "avatars": [
    {
      "id": "default-chibi",
      "outfit": "default",
      "proportion": "chibi",
      "title": { "en": "…", "ja": "…" },
      "version": 1,
      "url": "https://github.com/…/releases/download/default-chibi-v1/default-chibi-v1.tar",
      "sha256": "…",
      "size": 12345678,
      "thumbnail": "https://github.com/…/releases/download/catalog/default-chibi.webp"
    }
  ]
}
```

- `scripts/catalog` は `catalog.toml` の項目ごとに、いちばん新しい `<id>-vN` のリリースから
  `version`・`url`・`sha256`・`size` を埋める。リリースのないアバターは載せない。
- 見本の画像（`thumbnails/<id>.webp`）は `catalog` のリリースに一緒に添付する。
- `schema` は形式の版。読む側は、知らない版なら読まない。項目を足すだけなら上げない。

### 大きくなったら

衣装が増えてリポジトリが 1 GB に近づいたら、`avatars/` のバイナリを git から外し、
リリースの添付だけで配る形に移る。tar とカタログの形は変わらないので、agent には影響しない。

## §2 検証（`scripts/check`）

PR ごとと、リリースの前に CI で走らせる。引数なしなら全アバター、`<id>` を渡せばその 1 つ。
手元でも同じに動くよう、**Python の標準ライブラリだけ**で書く（TOML は `tomllib`、
PNG・WebP・glTF バイナリは自前で読む）。exiftool などの外部ツールは使わない。

### メタデータ

編集・書き出しツールは、作成ツール名・設定・作者名などを画像やモデルに埋め込む。
公開する素材にはこれを残さない。危ないものを列挙するのではなく、**安全と分かっているものだけを
許し、それ以外があれば失敗する**。将来のツールが新しい種類を足しても素通りしないようにするため。
見本の画像（`thumbnails/`）も対象。

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
  - `asset.generator` は、決めた値（`sapphire-agent-avatars`）と一致すること。
- **`profile.md`**: UTF-8 のテキストであること。

### アバターの中身

- `avatar.toml` が読め、既存の規則を満たす（`default_pose` が `poses` にある、表情の名前が
  `SPRITE_EXPRESSIONS` のどれか、など）。意味の正しさの基準は agent の
  `AvatarConfig::check` で、agent も展開後に検証する（§3）ので、ここで完全に再現しなくてよい。
- `avatar.toml` が参照するファイルがすべてあり、`avatars/<id>/` の下に
  **参照されていないファイルがない**。
- 立ち絵はすべて同じキャンバスサイズ（2D の仕様が前提にしている）。
- アバター 1 つ（`profile.md` を含めて）が 100 MB 以下、1 ファイルが `AVATAR_MAX_FILE`
  （256 MiB）以下。

### リポジトリ全体

- `avatars/` のディレクトリと `catalog.toml` の項目が 1 対 1 で対応する。
- `id` の形（§1）と、`outfit`・`proportion` が `id` と食い違わないこと。
- 各アバターに見本の画像がある。

### 掃除（`scripts/strip`）

- PNG・WebP から、許可したチャンク以外を取り除いて書き直す。
- VRM・VRMA の `asset.generator` を決めた値に書き換える（JSON チャンクだけを書き直し、
  バイナリチャンクには触れない）。編集ツールの多くはこの値を変えられないため。
- VRM の `meta`、各種の名前、モデルに埋め込むテクスチャは、編集ツールの側で直す
  （テクスチャは取り込む前に PNG・WebP として掃除しておく）。直し忘れは `scripts/check` が捕まえる。

## §3 agent のインストーラー

`crates/sapphire-agent-server/src/default_avatar.rs` を新設する。

```rust
/// One avatar release: where to get it and what it must hash to.
pub(crate) struct AvatarRelease<'a> {
    pub id: &'a str,
    pub version: u32,
    pub url: &'a str,
    pub sha256: &'a str,
}

/// The avatar a new workspace gets, pinned.
pub(crate) const INITIAL: AvatarRelease<'static> = AvatarRelease {
    id: "…", // decided with the first release
    version: 1,
    url: "https://github.com/sapphire-turtle/sapphire-agent-avatars/releases/download/<id>-v1/<id>-v1.tar",
    sha256: "…", // the release's `.sha256`, filled in when it is released
};

pub(crate) enum Outcome {
    Installed,
    /// `avatar/avatar.toml` was already there; nothing was touched.
    AlreadyPresent,
}

pub(crate) fn install(
    workspace_root: &Path,
    release: &AvatarRelease<'_>,
    progress: impl FnMut(u64 /* received */, Option<u64> /* total */),
) -> anyhow::Result<Outcome>;
```

- 初回は `INITIAL` を渡す。初回のアバターを替えるのは、`INITIAL` を書き換える PR。
- 追加のアバター（ショップ）は、カタログの項目から `AvatarRelease` を作って同じ関数を呼ぶ想定。
  カタログを読む部分はショップと一緒に作る。
- 入れる先は今の `avatar/` 1 つ。複数のアバターを置けるようになったら（別の issue）、
  入れる先をアバターの ID で分ける。

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

- 正しい tar が入り、`Installed` を返す
- sha256 が違うと失敗し、`avatar/` に何も残らない
- `avatar.toml` がすでにあると `AlreadyPresent` で、何も変わらない
- `../` を含むエントリー、`avatar/` の外のエントリー、シンボリックリンクのエントリーで失敗する
- `avatar.toml` が参照するファイルが欠けていると失敗する
- 上限を超えるファイルで失敗する
- 失敗のあとに `avatar/` が半端な状態で残らない

## 未決のこと

- 素材リポジトリの名前（仮称 `sapphire-agent-avatars`）
- 初回に入るアバター（`default-regular` か `default-chibi` か。初回起動で選ばせる案もある）
- 最初のポーズとアクションのセット（素材の制作と合わせて決める）
- アバター 1 つの上限 100 MB の妥当性（VRM の実寸を見て見直す）
