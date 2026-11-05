# デフォルトキャラクターの素材（素材リポジトリと、agent のインストーラー）

- **対象**: 新設リポジトリ `sapphire-turtle/sapphire-agent-avatars`、
  `crates/sapphire-agent-server/`（新設モジュール `default_avatar`、`cli_init`、IPC メソッド）、
  `crates/sapphire-agent-core/`（IPC の型）、`crates/sapphire-agent-gui/`（起動時の取得）
- **参照**: `crates/sapphire-agent-core/src/lib.rs` の `AvatarConfig` と `AVATAR_MAX_FILE`、
  `crates/sapphire-agent-server/src/avatar_store.rs`（パスの検証と、`avatar/` の外での一時置き）、
  `crates/sapphire-agent-server/src/voice/providers/sherpa_download.rs`（取得と展開の前例）、
  [デスクトップマスコットの仕様](2026-10-28-desktop-companion-design.md)
- **関連 issue**: #340（デフォルトキャラクターと初回起動の流れ）のやること 1・3・4。
  ワークスペースの既定の場所は sapphire-turtle/sapphire-framework#398（framework 0.26.0）。
  1 つのワークスペースに複数のアバターを置いて切り替える仕組みは別の issue で扱う
- **状態**: 設計

## なぜ

デスクトップマスコットは、ユーザーが VRM モデルや立ち絵を用意しないとキャラクターが出ない。
デフォルトのオリジナルキャラクターを用意し、アバターを用意していなくてもすぐに使えるようにする（#340）。

#340 で決めた前提:

- 素材はソースコードと別のリポジトリに置き、GitHub Releases で配布する。
- 取り込んだアバターはユーザーが置いた素材と同じ扱いで、削除も差し替えも自由。
- 自動アップグレードはしない。新しい版が効くのは、その後に取得したものだけ。
- オフライン用の代替は埋め込まない（アプリはオンラインが前提）。
- 人格はブートストラップでユーザーが決め、`IDENTITY.md` / `SOUL.md` に書く。
  agent が配るのは見た目と、名前・口調などの**案**だけ。
- ワークスペースの同期は 1 GiB まで。

その後に決めたこと:

- デフォルトキャラクターは 1 人。ただし**通常の等身とデフォルメの等身**の 2 種類を、
  2D・3D とも用意する。
- 衣装は少しずつ増やす（季節の衣装など）。初回に入るのは 1 つだけで、ほかはあとから
  追加で取得する（無料の「ショップ」。別に扱う）。
- **キャラクター設定とアバターは、入るタイミングを分ける**（§4）。
  - キャラクター設定（案）は、ワークスペースの初期化（`init`）で、テンプレートと一緒に置く。
    agent に埋め込んであるので、ネットワークは使わない。
  - アバターは、**GUI が起動したときにアバターが 1 つもなければ**取得する。
  - 理由: CLI だけで使う人にアバターは要らない。逆に GUI では、アバターを消したり
    取得に失敗したりすると表示するものがなくなるので、そのときに取り直せるほうがよい。
  - このため、#340 の「新規ワークスペースの初期化時に一度だけ取得する」と
    「既存のワークスペースには適用しない」は改める。アバターのないワークスペースを GUI で
    開けば、既存のものでもデフォルトのアバターが入る。アバターのある
    ワークスペースには何もしない（自動アップグレードはしない、は変わらない）。

## 範囲

この仕様が扱うのは次の 3 つ。

1. 素材リポジトリ: 構成、ライセンス、検証、リリースとカタログの形式（§1・§2）
2. agent 側の、アバターを 1 つ取得して `avatar/` に入れる部品（§3）
3. それを使う 2 つの入口: `init` でのキャラクター設定と、GUI の起動時のアバターの取得（§4）

扱わないもの:

- `init` の既定の場所の追従（#340 のやること 7）
- `BOOTSTRAP.md` の改訂（やること 5）。この仕様は `CHARACTER.md` を置くところまで
- GUI の初回起動画面（join するか新規に初期化するかの選択。やること 6）
- ショップの画面と、カタログを読んで追加のアバターを入れる流れ
- 1 つのワークスペースに複数のアバターを置き、選んで切り替える仕組み（別の issue）。
  それまでは、入れる先は今の `avatar/` 1 つ
- 素材そのものの制作

## 用語

- **キャラクター**: デフォルトのオリジナルキャラクター（1 人）。
- **キャラクター設定**: キャラクターの名前・口調・一人称・雰囲気の案。キャラクター単位で 1 つ。
  ワークスペースでは `CHARACTER.md`（§4.1）。
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
| キャラクター設定 | agent に `templates/workspace/CHARACTER.md` として埋め込み、`init` で置く |
| アバターを入れる契機 | GUI がサーバーにつないだとき、アバターが 1 つもなければ。CLI と headless は取得しない |
| 同期との関係 | 同期が有効で、ほかの端末がある場合は自動で取らず、ボタンで取る |

### リポジトリを分けない理由

- ショップには「何があるか」の一覧が要る。1 つのリポジトリなら `catalog.json` 1 つで済む。
  アバターごとのリポジトリだと、GitHub API でリポジトリを列挙することになり、
  レート制限や、どれが公開してよいものかの区別が問題になる。
- LICENSE、`scripts/check`、CI はキャラクターで共通。リポジトリを分けると
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
- キャラクター設定（名前・口調などの案）はこのリポジトリに置かず、tar にも入れない。
  agent に埋め込み、`init` でワークスペースに置く（§4.1）。

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
  2. `avatars/<id>/` を `avatar/` として tar にまとめ、`<id>-vN.tar` と `<id>-vN.tar.sha256` をリリースに添付する。
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

### アバターの中身

- `avatar.toml` が読め、既存の規則を満たす（`default_pose` が `poses` にある、表情の名前が
  `SPRITE_EXPRESSIONS` のどれか、など）。意味の正しさの基準は agent の
  `AvatarConfig::check` で、agent も展開後に検証する（§3）ので、ここで完全に再現しなくてよい。
- `avatar.toml` が参照するファイルがすべてあり、`avatars/<id>/` の下に
  **参照されていないファイルがない**。
- 立ち絵はすべて同じキャンバスサイズ（2D の仕様が前提にしている）。
- アバター 1 つが 100 MB 以下、1 ファイルが `AVATAR_MAX_FILE`
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

/// The avatar a workspace without one gets, pinned. `None` until the
/// first avatar is released: there is nothing to fetch yet.
pub(crate) const INITIAL: Option<AvatarRelease<'static>> = Some(AvatarRelease {
    id: "…", // decided with the first release
    version: 1,
    url: "https://github.com/sapphire-turtle/sapphire-agent-avatars/releases/download/<id>-v1/<id>-v1.tar",
    sha256: "…", // the release's `.sha256`
});

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

- デフォルトのアバターは `INITIAL` を渡す（§4.2）。替えるのは、`INITIAL` を書き換える PR。
- 最初のアバターがリリースされるまで `INITIAL` は `None`。仮の値（全部 0 の sha256 や
  `placeholder-chibi-v1`）は入れない。前者は取得のたびに失敗し、後者はその間にアプリが
  リリースされると、仮の素材が利用者に届いてしまう。
- 追加のアバター（ショップ）は、カタログの項目から `AvatarRelease` を作って同じ関数を呼ぶ想定。
  カタログを読む部分はショップと一緒に作る。
- 入れる先は今の `avatar/` 1 つ。複数のアバターを置けるようになったら（別の issue）、
  入れる先をアバターの ID で分ける。

### 流れ

1. `<workspace>/avatar/avatar.toml` があれば、何もせず `AlreadyPresent` を返す
   （テンプレートの展開と同じく、上書きしない）。
2. キャッシュディレクトリの一時ファイルへ進捗つきで取得し（`sherpa_download.rs` のように
   全体をメモリに載せない）、取得後にそのファイルの sha256 を計算する。`progress` に受信量と
   全体の大きさ（`Content-Length` があれば）を渡す。sha256 が違えば失敗。
3. キャッシュディレクトリの作業用ディレクトリに展開する（§3.1 の規則）。ワークスペースの
   外に置くのは、展開途中の状態を同期に見せないため。
4. 作業用ディレクトリで、`avatar.toml` が `AvatarConfig` として読めて `check` を通り、
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

`install` は失敗を返すだけで、ワークスペースには何も残さない（§3 の流れ 5・6）。冪等なので、
やり直しはもう一度呼ぶだけ。失敗したときの見せ方とやり直しの入口は §4.2。

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

## §4 入口

### 4.1 `init`: キャラクター設定

- キャラクター設定を `crates/sapphire-agent-server/templates/workspace/CHARACTER.md` に置き、
  `cli_init::TEMPLATES` に `CHARACTER.md`（ワークスペースのルート）として足す。
  ほかのテンプレートと同じく `include_str!` で埋め込み、上書きはしない。
- `init` はネットワークを使わない。アバターも取りに行かない。
- `CHARACTER.md` は案であって設定ではない。agent はシステムプロンプトに読み込まず
  （`workspace.rs` の読むファイルに足さない）、ブートストラップが提案の材料として読むだけ
  （`BOOTSTRAP.md` の改訂は #340 のやること 5）。`avatar/` の外に置くので、アバターを
  消しても差し替えても残る。
- 中身（名前・口調・一人称・雰囲気の案）はキャラクターの制作と一緒に書く。中身ができるまでは
  `TEMPLATES` に足さない。空のひな形を利用者に配らないため。
- ライセンスは素材と同じ CC0 1.0。コードの GPL とは別なので、ファイルの先頭と README の
  ライセンス表に書く。
- 正本は agent のリポジトリ。素材リポジトリにはキャラクター設定を置かない（今ある
  `character/profile.md` は消す）。

### 4.2 GUI: アバターがないときの取得

**判定**: GUI がサーバーにつないだとき（起動時と、ワークスペースを切り替えてつなぎ直したとき）に
`avatar.manifest` を読み、`config` がなければ（`avatar/avatar.toml` がなければ）
「アバターが 1 つもない」とみなす。複数のアバターを置けるようになったら（#369）、
そのどれもないときに読み替える。

**IPC メソッド**（`sapphire-agent-core` に定数と型、サーバーに実装。`APP_API` を上げる）:

- `sapphire-agent.avatar.install_default`（要求）: パラメーター `{ "auto": bool }`。
  取得が終わってから答える。答えは次のどれか。
  - `installed`: 入れた。
  - `already_present`: もうあった。何もしていない。
  - `unavailable`: `INITIAL` が `None`。
  - `needs_confirmation`: `auto` が真で、同期が有効かつほかの端末がある。何もしていない。
  - 取得や検証に失敗したら、IPC のエラーで答える（メッセージは `install` のエラー）。
- `sapphire-agent.avatar.install_progress`（通知）: `{ "received": u64, "total": u64 | null }`。
  取得中に、`install` の `progress` から送る。間隔は 250 ミリ秒に 1 回まで。
- サーバーは `install` を `spawn_blocking` で走らせる。同時に走るのは 1 つだけで、
  走っている間に来た要求は、その結果を待って同じ答えを返す。

**同期との関係**: 既存のワークスペースに join したばかりの端末では、アバターが同期でまだ届いて
いないだけのことがある。そこで取ると、届いたファイルとぶつかる。framework の `sync.status` は
`enabled` とほかの端末の数（`peers`）しか教えず、「最初の同期が追いついた」は分からないので、
次のように決める。

- 同期が無効、または `peers` が 0: 自動で取る。
- 同期が有効で `peers` が 1 以上: 自動では取らない（`needs_confirmation`）。
- 判定はサーバーが `install_default` の中で行う（テストできるように）。framework に
  「追いついた」が分かる手段ができたら、それを待ってから自動で取るように改める。

**GUI の見せ方**: ゲームの初回ダウンロードのように、取得が終わるまでローディング画面で待たせる。

- つないだときにアバターがなければ、`install_default { auto: true }` を呼ぶ。
- 答えを待つ間は、ウィンドウ全体をローディング画面にする。マスコットのシルエットも
  チャットも出さない。画面には「キャラクターをダウンロードしています」と、進み具合の
  バー（受信量 / 全体。`total` がなければ受信量だけで、バーは不定）を出す。
- `installed` なら、アバターを読み直してから通常の画面に移る。
- 失敗したら、ローディング画面のままエラーのメッセージと、「再試行」
  （`install_default { auto: false }`）・「アバターなしで続ける」の 2 つのボタンを出す。
  オフラインでもアプリを使えるように、後者は必ず出す。選ぶと今の仮表示（シルエット）で
  通常の画面に移り、そのつなぎの間はもう自動で取りに行かない。
- `needs_confirmation`・`unavailable` なら、ローディング画面を出さずに仮表示で通常の画面に
  移る。`needs_confirmation` は、アバターが同期で届く見込みがあるため。
- 仮表示のときは、「キャラクター」設定ページに「デフォルトのアバターを取得」ボタンを出す
  （`unavailable` なら出さない）。押すと同じローディング画面に移る。
- アバターを意図して「なし」にしておく設定は作らない。必要になったら足す。

**headless**: GUI がないので、このメソッドを呼ぶものがない。headless は取得しない。

### 4.3 テスト

- `init` が `CHARACTER.md` を置き、すでにあれば上書きしない（中身ができて `TEMPLATES` に
  足すときに足す）。`init` がネットワークを使わないことは、`default_avatar` を呼ばないことで保つ。
- `install_default` の判定: 同期が無効、`peers` が 0、`peers` が 1 以上 × `auto` の真偽で、
  取りに行くかどうかと答え。`INITIAL` が `None` なら `unavailable`。取得先は差し替えられる
  ようにし、テストではループバックの HTTP サーバー（§3 のテストと同じもの）を使う。
- 同時に 2 つの要求が来ても、取得は 1 回で、両方に同じ答えが返る。
- 進み具合の通知が、`total` を含めて届く。
- GUI: マニフェストに `config` がないときだけ `install_default` を呼び、答えを待つ間はローディング画面、
  入ったら読み直して通常の画面、失敗なら 2 つのボタン、`needs_confirmation` なら仮表示。
  画面の状態の移り変わりを、描画と分けた関数で確かめる。

## 未決のこと

- 初回に入るアバター（`default-regular` か `default-chibi` か。初回起動で選ばせる案もある）
- 最初のポーズとアクションのセット（素材の制作と合わせて決める）
- アバター 1 つの上限 100 MB の妥当性（VRM の実寸を見て見直す）
