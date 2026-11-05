# Avatar Asset Repository Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The tooling of the public avatar repository: metadata and content checks, a cleaner, a reproducible tar builder, the catalog generator, and the CI that releases `<id>-vN` tars and the `catalog.json`. No art is part of this plan.

**Architecture:** A stdlib-only Python package `tools/avatars/` holds all logic (one module per format plus `avatar`, `repo`, `pack`, `catalog`); the executables in `scripts/` are thin argument parsers over it. Tests in `tests/` build their fixture files in code (PNG, WebP, glb), so no binary fixtures are committed. GitHub Actions run the tests and `scripts/check` on PRs, and build and publish on tags.

**Tech Stack:** Python ≥ 3.11 (`tomllib`, `tarfile`, `hashlib`, `unittest`), GitHub Actions, `gh` CLI.

**Spec:** `docs/superpowers/specs/2026-11-05-default-avatar-pack-design.md` in sapphire-agent (§1, §2)

## Global Constraints

- Work in a local repo at `../sapphire-agent-avatars` (sibling of sapphire-agent; working name). **Creating the GitHub repo and pushing are not part of the plan** — that waits for the user's go-ahead and the final name.
- Python standard library only; no third-party packages, no external tools (no exiftool, no GNU tar).
- Nothing in the repo — code, comments, README, commit messages — describes how the assets are made or which tools made them. Commit author is the user's public identity; commit messages are plain.
- Code and comments in English. Commit messages in conventional style (`feat:`, `test:`, `ci:`).
- `ID_RE = r"^[a-z0-9]+(?:-[a-z0-9]+)*-(regular|chibi)$"`; outfit = id without the last `-<proportion>`.
- Path rule for files named in `avatar.toml` (mirrors the agent's `validate_path`): non-empty, `/`-separated, no leading `/`, no `\` or `:`, no empty/`.`/`..` component, no component starting with `.` or ending in `.uploading`.
- `SPRITE_EXPRESSIONS = ("happy", "angry", "sad", "relaxed", "surprised")`.
- Limits: `MAX_FILE = 256 * 1024 * 1024`, `MAX_AVATAR_TOTAL = 100_000_000` (avatar dir + `profile.md`) — the same numbers the agent enforces.
- PNG chunk allowlist: `IHDR PLTE IDAT IEND tRNS sRGB gAMA pHYs`. WebP chunk allowlist: `VP8 ` `VP8L` `VP8X` `ALPH` `ANIM` `ANMF`.
- `GENERATOR = "sapphire-agent-avatars"`.
- Expected VRM 1.0 `extensions.VRMC_vrm.meta` — one constant `EXPECTED_META` in `tools/avatars/glb.py`, so changing a permission is a one-line edit:
  `authors = ["sapphire-agent"]`, `licenseUrl = "https://vrm.dev/licenses/1.0/"`, `otherLicenseUrl = "https://creativecommons.org/publicdomain/zero/1.0/"`, `avatarPermission = "everyone"`, `commercialUsage = "corporation"`, `creditNotation = "unnecessary"`, `allowRedistribution = true`, `modification = "allowModificationRedistribution"`, `allowExcessivelyViolentUsage = true`, `allowExcessivelySexualUsage = true`, `allowPoliticalOrReligiousUsage = true`, `allowAntisocialOrHateUsage = true`. Allowed other meta keys: `name`, `version`, `thumbnailImage`. Any other key (`contactInformation`, `references`, `copyrightInformation`, `thirdPartyLicenses`, …) fails.
- Release asset names: `<id>-v<N>.tar`, `<id>-v<N>.tar.sha256` (format: `<hex>  <id>-v<N>.tar\n`). Catalog release tag: `catalog`; its assets: `catalog.json`, `<id>.webp` per thumbnail.

## Review Focus

1. **glTF `extras` and `asset.copyright`** — exporters put custom properties and author strings there; the spec's allowlist intent means they must fail. Test: `glb_rejects_extras_anywhere` and `glb_rejects_copyright` (Task 3).
2. **An image referenced by URI instead of embedded** (`images[].uri`, including `data:` URIs) — it escapes the embedded-image check; expect failure. Test: `glb_rejects_image_uri` (Task 3).
3. **WebP `VP8X` flags left set after stripping** (ICC/EXIF/XMP bits) — expect `strip` to clear bits for the chunks it removed so decoders don't look for them. Test: `strip_webp_clears_vp8x_flags` (Task 2).
4. **A glb whose JSON chunk changes length when the generator is rewritten** — expect correct 4-byte space padding and updated chunk and total lengths, with the BIN chunk byte-identical. Test: `strip_glb_rewrites_generator_and_keeps_bin` (Task 3).
5. **Tag for an id that is in `avatars/` but missing from `catalog.toml`, or a tag that does not match `ID_RE`** — expect the release job to fail before publishing anything. Test: `pack_rejects_unknown_id` (Task 5).

---

### Task 1: Repository skeleton

**Files:**
- Create: `LICENSE` (CC0 1.0 legal code, verbatim), `README.md` (sections: what this is, avatar list, license — no production details), `.gitignore` (`__pycache__/`, `/dist/`), `catalog.toml` (empty: a comment only), `character/profile.md` (placeholder headings: Name / Tone / First person / Vibe, each "TBD"), `avatars/.gitkeep`, `thumbnails/.gitkeep`, `tools/avatars/__init__.py`, `tests/__init__.py`
- Create: `.github/workflows/ci.yml` — on `pull_request` and `push` to `main`: `actions/setup-python` 3.11, `python -m unittest discover -s tests`, `python scripts/check`

- [ ] **Step 1:** `git init ../sapphire-agent-avatars`, create the files.
- [ ] **Step 2: Run** `python -m unittest discover -s tests` — "Ran 0 tests", OK.
- [ ] **Step 3: Commit** `chore: repository skeleton`.

### Task 2: PNG and WebP — check, strip, dimensions

**Files:**
- Create: `tools/avatars/png.py`, `tools/avatars/webp.py`, `tests/fixtures.py`, `tests/test_images.py`

**Interfaces:**
- Produces (both modules, same shape):
  - `def chunks(data: bytes) -> list[tuple[bytes, bytes]]` — `(type, payload)`; raises `ValueError` on a bad signature/length.
  - `def check(data: bytes) -> list[str]` — problems, empty when clean (each names the offending chunk).
  - `def strip(data: bytes) -> bytes` — only allowlisted chunks kept; PNG CRCs preserved per chunk; WebP RIFF size recomputed, odd payloads padded, and `VP8X` flag bits cleared for ICC (0x20), EXIF (0x08), XMP (0x04).
  - `def size(data: bytes) -> tuple[int, int]` — PNG from `IHDR`; WebP from `VP8X` canvas, else `VP8 `/`VP8L` frame header.
- `tests/fixtures.py`: `png(w, h, extra_chunks=[])`, `webp_lossless(w, h, extra_chunks=[], vp8x_flags=None)` building minimal valid files (PNG: zlib-compressed IDAT of zero rows; WebP: a `VP8L` header with the given dimensions is enough for these parsers).

- [ ] **Step 1: Write failing tests:** `png_clean_passes`; `png_rejects_text` (each of `tEXt zTXt iTXt eXIf caBX iCCP` → one problem naming it); `png_strip_removes_disallowed_and_passes_check`; `png_size`; `webp_clean_passes`; `webp_rejects_metadata` (`EXIF`, `XMP `, `ICCP`); `webp_strip_removes_and_passes_check`; `strip_webp_clears_vp8x_flags`; `webp_size_vp8x_and_vp8l`; `rejects_garbage` (`ValueError`).
- [ ] **Step 2: Run** `python -m unittest tests.test_images` — fails (modules missing).
- [ ] **Step 3: Implement** both modules.
- [ ] **Step 4: Run** — pass.
- [ ] **Step 5: Commit** `feat: PNG and WebP metadata check and strip`.

### Task 3: glb (VRM / VRMA) — check and generator rewrite

**Files:**
- Create: `tools/avatars/glb.py`, `tests/test_glb.py`; add `glb(json_obj, bin_bytes=b"")` to `tests/fixtures.py`

**Interfaces:**
- Consumes: `png.check`, `webp.check`.
- Produces:
  - `def read(data: bytes) -> tuple[dict, bytes]` — JSON and BIN chunk; `ValueError` on bad magic/version/lengths.
  - `def write(gltf: dict, bin_: bytes) -> bytes` — JSON compact, space-padded to 4 bytes; BIN zero-padded; header lengths correct.
  - `def check(data: bytes, kind: str) -> list[str]` — `kind` is `"vrm"` or `"vrma"`.
  - `def strip(data: bytes) -> bytes` — sets `asset.generator = GENERATOR`, changes nothing else; BIN bytes identical.

`check` rules: `asset.generator == GENERATOR`; no `asset.copyright`; no `extras` key at any depth; every `images[]` has `bufferView` and `mimeType` in (`image/png`, `image/webp`) and no `uri`, and its bytes pass the matching image `check`; every `name` in `images textures materials meshes nodes skins animations samplers` matches `^[A-Za-z0-9_-]{1,64}$`; for `"vrm"`, `extensions.VRMC_vrm.meta` equals `EXPECTED_META` on its keys and has no keys outside it plus the allowed three; for `"vrma"`, `extensions.VRMC_vrm_animation` is present.

- [ ] **Step 1: Write failing tests:** `glb_clean_vrm_passes`; `glb_clean_vrma_passes`; `glb_rejects_wrong_generator`; `glb_rejects_copyright`; `glb_rejects_extras_anywhere` (nested in a material); `glb_rejects_image_uri`; `glb_rejects_embedded_png_with_text`; `glb_rejects_bad_name` (`"Body.001"`, a 65-char name, a non-ASCII name); `glb_rejects_meta_mismatch` (one permission flipped); `glb_rejects_extra_meta_key` (`contactInformation`); `vrm_without_vrmc_vrm_fails`; `strip_glb_rewrites_generator_and_keeps_bin` (generator of a length that changes padding).
- [ ] **Step 2: Run** `python -m unittest tests.test_glb` — fails.
- [ ] **Step 3: Implement** `glb.py`.
- [ ] **Step 4: Run** — pass.
- [ ] **Step 5: Commit** `feat: glb metadata check and generator rewrite`.

### Task 4: Avatar and repository checks; `scripts/check` and `scripts/strip`

**Files:**
- Create: `tools/avatars/avatar.py`, `tools/avatars/repo.py`, `scripts/check`, `scripts/strip`, `tests/test_avatar.py`, `tests/test_repo.py`

**Interfaces:**
- Consumes: `png`, `webp`, `glb` (`check`, `strip`, `size`).
- Produces:
  - `avatar.check(dir: Path, profile: Path) -> list[str]` — `avatar.toml` parses; structural rules (`[vrm]`: non-empty `model`, `idle`, action names/paths; `[sprite]`: non-empty `poses`, `default_pose` among them, expression keys in `SPRITE_EXPRESSIONS`); every referenced path obeys the path rule and exists; no file under `dir` is unreferenced (besides `avatar.toml`); every file passes its format check by extension (`.png`, `.webp`, `.vrm` → `"vrm"`, `.vrma` → `"vrma"`; any other extension fails); all sprites share one `size`; each file ≤ `MAX_FILE`; total + `profile` ≤ `MAX_AVATAR_TOTAL`; `profile` decodes as UTF-8.
  - `repo.load_catalog(root: Path) -> list[dict]` — `catalog.toml` `[[avatar]]` entries (`id`, `outfit`, `proportion`, `title.en`, `title.ja`).
  - `repo.check(root: Path, only: str | None = None) -> list[str]` — every id matches `ID_RE`; `outfit`/`proportion` agree with the id; ids unique; the set of `avatars/*` dirs equals the set of catalog ids; each has `thumbnails/<id>.webp` that passes `webp.check`; then `avatar.check` for each (or only `only`). Problems are prefixed with the id.
  - `scripts/check [id]` — prints problems, exits 1 if any. `scripts/strip <file>...` — rewrites PNG/WebP/VRM/VRMA in place via `strip`; refuses other extensions.
- The scripts add `<repo>/tools` to `sys.path` from their own location.

- [ ] **Step 1: Write failing tests** with a temp repo built in code (`fixtures.repo(tmp, ids=[...])` writing `catalog.toml`, a valid avatar dir with one sprite pose and a VRM, a thumbnail and `profile.md`): `valid_repo_passes`; `unreferenced_file_fails`; `missing_referenced_file_fails`; `sprite_sizes_differ_fails`; `bad_expression_key_fails`; `path_with_dotdot_fails`; `unknown_extension_fails`; `total_over_cap_fails` (patch `MAX_AVATAR_TOTAL` small); `dir_without_catalog_entry_fails`; `catalog_entry_without_dir_fails`; `id_proportion_mismatch_fails`; `bad_id_fails`; `missing_thumbnail_fails`; `profile_not_utf8_fails`; `check_script_exit_codes` (subprocess `scripts/check` on valid → 0, on broken → 1).
- [ ] **Step 2: Run** `python -m unittest discover -s tests` — new tests fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — pass; `python scripts/check` on the real (empty) repo → exit 0.
- [ ] **Step 5: Commit** `feat: avatar and repository checks`.

### Task 5: Reproducible tar and the catalog

**Files:**
- Create: `tools/avatars/pack.py`, `tools/avatars/catalog.py`, `scripts/pack`, `scripts/catalog`, `tests/test_pack.py`, `tests/test_catalog.py`

**Interfaces:**
- Consumes: `repo.load_catalog`, `repo.check`.
- Produces:
  - `pack.parse_tag(tag: str) -> tuple[str, int]` — `"<id>-v<N>"` split at the last `-v`; `ValueError` unless the id matches `ID_RE` and N ≥ 1.
  - `pack.build(root: Path, id: str, version: int, out_dir: Path) -> Path` — fails (`ValueError`) if the id is not in the catalog or `repo.check(root, only=id)` reports anything; writes `<id>-v<N>.tar` with `tarfile` `format=USTAR_FORMAT`: entries `avatar/` and its subdirectories, every file of `avatars/<id>/` under `avatar/`, plus `character/profile.md` as `avatar/profile.md`; sorted by name; `mtime=0`, `uid=gid=0`, `uname=gname=""`, mode `0o755` dirs / `0o644` files. Writes `<id>-v<N>.tar.sha256`.
  - `catalog.build(root: Path, releases: list[dict], sums_dir: Path, repo_url: str) -> dict` — `releases` is `gh api repos/{repo}/releases --paginate` output (uses `tag_name`, `assets[].name`, `assets[].size`, `assets[].browser_download_url`); for each catalog entry in order, the highest-N `<id>-vN` release that has the tar asset; `sha256` from `sums_dir/<id>-vN.tar.sha256`; entries without a release are omitted; output shape exactly as spec §1.1 with `"schema": 1`, `thumbnail = f"{repo_url}/releases/download/catalog/{id}.webp"`.
  - `scripts/pack <tag> <out_dir>`; `scripts/catalog --releases <json> --sums <dir> --repo-url <url> --out <file>`.

- [ ] **Step 1: Write failing tests:** `parse_tag_ok` (`"santa-chibi-v12"` → `("santa-chibi", 12)`); `parse_tag_rejects` (`"catalog"`, `"default-chibi-v0"`, `"Default-chibi-v1"`, `"default-tall-v1"`); `pack_is_reproducible` (two builds, identical bytes); `pack_layout_and_headers` (names start with `avatar/`, includes `avatar/profile.md`, all uid 0, empty uname, mtime 0); `pack_rejects_unknown_id`; `pack_rejects_failing_check`; `sha256_file_format`; `catalog_picks_highest_version`; `catalog_keeps_catalog_order_and_omits_unreleased`; `catalog_shape` (keys equal the spec's list).
- [ ] **Step 2: Run** — fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — pass.
- [ ] **Step 5: Commit** `feat: reproducible avatar tar and catalog generator`.

### Task 6: Release workflow and README

**Files:**
- Create: `.github/workflows/release.yml`
- Modify: `README.md` (avatar list table generated by hand from `catalog.toml`; license section stating CC0 for everything in the repo; how to verify a tar with its `.sha256`)

`release.yml`, on `push` of tags `*-v*`, `permissions: contents: write`, one job:
1. Checkout, setup Python 3.11, `python -m unittest discover -s tests`.
2. `python scripts/pack "$GITHUB_REF_NAME" dist` (fails the job for an unknown id or failing check, before anything is published).
3. `gh release create "$GITHUB_REF_NAME" dist/* --title "$GITHUB_REF_NAME" --notes ""`.
4. `gh release view catalog || gh release create catalog --title catalog --notes "Avatar catalog"`.
5. `gh api repos/$GITHUB_REPOSITORY/releases --paginate > releases.json`; download every `*-v*.tar.sha256` asset into `sums/` (`gh release download <tag> -p '*.tar.sha256' -D sums` per tag found in `releases.json`).
6. `python scripts/catalog --releases releases.json --sums sums --repo-url "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY" --out dist-catalog/catalog.json`; copy `thumbnails/*.webp` into `dist-catalog/`.
7. `gh release upload catalog dist-catalog/* --clobber`.

Add a `concurrency: { group: catalog, cancel-in-progress: false }` so two tag pushes cannot interleave catalog uploads.

- [ ] **Step 1:** Write the workflow and README.
- [ ] **Step 2: Verify:** `actionlint .github/workflows/*.yml` if installed (no output = pass); otherwise check the workflow step by step against the list above. Re-run `python -m unittest discover -s tests` and `python scripts/check` — pass. The workflow itself first runs for real on the first tag after the repo is published.
- [ ] **Step 3: Commit** `ci: release avatar tars and the catalog on tags`.
