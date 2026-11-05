# Default Avatar Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `default_avatar` module in the agent server that downloads one avatar release (a tar), verifies its sha256, extracts it safely and places it into `<workspace>/avatar/`.

**Architecture:** Three layers, each testable on its own: `extract` (tar reader → a staging directory, with the safety rules), `install_archive` (a tar file on disk → verified, checked, placed into the workspace), and `install` (download with progress, then `install_archive`). Only `fetch` touches the network; its tests use a loopback HTTP server. Nothing calls `install` yet — wiring it into `init` is #340 task 2.

**Tech Stack:** Rust, `tar` 0.4 (made non-optional), `reqwest` blocking, `sha2`, `dirs`.

**Spec:** `docs/superpowers/specs/2026-11-05-default-avatar-pack-design.md` (§3)

## Global Constraints

- All code lives in `crates/sapphire-agent-server/src/default_avatar.rs`; register it as `mod default_avatar;` in `src/lib.rs`.
- Commit scope: `feat(workspace)` / `test(workspace)` style — agent-internal, no `cliff.toml` change.
- Code and comments in English.
- Limits: one file ≤ `sapphire_agent_core::AVATAR_MAX_FILE` (256 MiB); one avatar's extracted total ≤ `MAX_AVATAR_TOTAL = 100_000_000` bytes (the spec's "100 MB"; the asset repo's check uses the same number).
- The archive's top-level directory is `avatar/`; it is stripped on extraction.
- Never overwrite: if `<workspace>/avatar/avatar.toml` exists, do nothing.
- Nothing partial may appear under `<workspace>/avatar/`: staging happens outside the workspace, and `avatar.toml` is placed **last** so its presence means "complete".
- Must hold on Windows: paths built from `/`-separated validated strings, never from raw archive paths.
- Until #340 task 2 calls it, mark the module `#![cfg_attr(not(test), allow(dead_code))]` so clippy stays clean.

## Review Focus

1. **A tar whose declared entry size lies** (header says 10 bytes, more follows, or a huge size) — expect the per-file and total caps to be enforced on bytes actually written, not just on the header. Test: `extract_rejects_total_over_cap` (Task 1) uses many files each under the per-file cap.
2. **A server that streams forever or sends far more than an avatar** — expect `fetch` to abort once received bytes exceed `MAX_DOWNLOAD`, and to remove the partial file. Test: `fetch_aborts_past_cap` (Task 3).
3. **An existing but empty `avatar/` directory, or one holding user files but no `avatar.toml`** — expect install to proceed, placing files beside existing ones, and to fail rather than clobber a same-named user file. Test: `install_refuses_to_overwrite_existing_file` (Task 2).
4. **A failure halfway through placing files** (e.g. a name clash on the 3rd file) — expect every file this call placed to be removed again and `avatar.toml` never to appear. Covered by the same test.
5. **Stale staging directories from a crashed earlier run** — expect each call to use a fresh unique staging directory and to remove it on success and on failure. Test: `staging_dir_is_removed` (Task 2).

---

### Task 1: Safe extraction

**Files:**
- Modify: `crates/sapphire-agent-server/Cargo.toml` (`tar` non-optional: drop `optional = true`, drop `"dep:tar"` from `voice-sherpa`, update the comment above it)
- Modify: `crates/sapphire-agent-server/src/lib.rs` (`mod default_avatar;`)
- Modify: `crates/sapphire-agent-server/src/avatar_store.rs` (make `sha256_file` `pub(crate)`)
- Create: `crates/sapphire-agent-server/src/default_avatar.rs`

**Interfaces:**
- Consumes: `avatar_store::validate_path(&str) -> Result<(), AvatarError>`, `AVATAR_MAX_FILE`.
- Produces:
  - `pub(crate) const MAX_AVATAR_TOTAL: u64 = 100_000_000;`
  - `fn extract(archive: impl std::io::Read, dest: &Path) -> anyhow::Result<()>` — `dest` exists and is empty; files land at `dest/<path without "avatar/">`.
  - Test helper `fn tar_of(entries: &[(&str, Kind, &[u8])]) -> Vec<u8>` with `enum Kind { File, Dir, Symlink(&'static str), HardLink(&'static str) }`. It writes each header's name bytes directly (`header.as_old_mut().name`) and calls `set_cksum`, bypassing `tar::Builder`'s own path checks so tests can produce hostile names like `avatar/../x`.

Rules `extract` enforces (from spec §3.1):
- Read the path with `entry.path_bytes()`; must be UTF-8; must be `avatar/` or start with `avatar/`. Strip that prefix and a trailing `/` on directories. Empty remainder (the `avatar/` dir itself) is skipped.
- Remainder must equal `avatar.toml` or pass `validate_path`.
- Entry type must be `Regular` (or `Continuous`) or `Directory`; anything else fails.
- Write regular files with `File::create` + `io::copy` from `entry.take(AVATAR_MAX_FILE + 1)`; fail if the written count exceeds `AVATAR_MAX_FILE`, or if the running total exceeds `MAX_AVATAR_TOTAL`. Do not use `entry.unpack` (no permissions or mtimes from the archive).

- [ ] **Step 1: Write the failing tests** in `default_avatar.rs` `mod tests`:
  - `extract_places_files_without_prefix`: entries `avatar/` (Dir), `avatar/avatar.toml`, `avatar/sprite/idle.webp` → `dest/avatar.toml` and `dest/sprite/idle.webp` with the same bytes.
  - `extract_rejects_parent_component`: `avatar/../evil` → `Err`, and `dest`'s parent has no `evil`.
  - `extract_rejects_outside_prefix`: `other/a.webp` → `Err`.
  - `extract_rejects_symlink` and `extract_rejects_hardlink`: `avatar/link` → `Err`.
  - `extract_rejects_hidden_name`: `avatar/.DS_Store` → `Err`.
  - `extract_rejects_total_over_cap`: build with a test-only cap — make the caps parameters of an inner `fn extract_with(archive, dest, max_file: u64, max_total: u64)` that `extract` calls with the real constants; the test uses `max_file = 10, max_total = 25` with three 10-byte files → `Err`; and `extract_rejects_file_over_cap` with one 11-byte file → `Err`.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server default_avatar` — expect compile failure (`extract` undefined).
- [ ] **Step 3: Implement** `extract` / `extract_with` per the rules above, plus the Cargo, `lib.rs` and `avatar_store` changes.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server default_avatar` — all pass. Also `cargo build -p sapphire-agent-server --no-default-features --features redb-store` to confirm `tar` is available without voice.
- [ ] **Step 5: Commit** `feat(workspace): extract a default avatar archive safely (#340)`.

### Task 2: Verify, check and place (`install_archive`)

**Files:**
- Modify: `crates/sapphire-agent-server/src/default_avatar.rs`
- Modify: `docs/superpowers/specs/2026-11-05-default-avatar-pack-design.md` (`AvatarConfig::validate` → `AvatarConfig::check`, twice)

**Interfaces:**
- Consumes: `extract`, `avatar_store::sha256_file(&Path) -> io::Result<String>` (lowercase hex), `AvatarConfig` (`toml::from_str`, `check() -> Result<(), String>`, `paths() -> Vec<&str>`), the copy-then-rename idea of `avatar_store::place_by_copy` (reimplement locally; do not widen its visibility).
- Produces:
  - `pub(crate) enum Outcome { Installed, AlreadyPresent }` (`Debug, PartialEq`)
  - `fn install_archive(workspace_root: &Path, archive: &Path, expected_sha256: &str, staging_root: &Path) -> anyhow::Result<Outcome>`

Flow:
1. `workspace_root/avatar/avatar.toml` exists → `Ok(AlreadyPresent)`, touch nothing.
2. `sha256_file(archive)` ≠ `expected_sha256` (compare case-insensitively) → `Err` naming both.
3. Create a unique staging dir under `staging_root` (`<pid>-<nanos since epoch>`), `extract` into it.
4. Read `avatar.toml` from staging (missing → `Err`), parse as `AvatarConfig`, `check()`, and every `paths()` entry exists as a file in staging.
5. Place: walk staging; for every file except `avatar.toml`, create parent dirs under `workspace_root/avatar/`, refuse if the destination already exists, then `rename` (on error: copy to a hidden `.<name>.installing` sibling, then rename). Record each placed path. Place `avatar.toml` last.
6. On any error in step 5, remove the files recorded so far. Always remove the staging dir (success or failure).

- [ ] **Step 1: Write the failing tests** (build archives with `tar_of`, write them to a tempdir, compute the expected hash with `sha256_file`). Use a minimal valid `avatar.toml`: `[sprite]\ndefault_pose = "idle"\n[sprite.poses.idle]\nbase = "sprite/idle.webp"\n`.
  - `install_archive_installs`: → `Ok(Installed)`; `avatar/avatar.toml` and `avatar/sprite/idle.webp` present; `avatar/profile.md` present when the archive has it.
  - `install_archive_skips_when_present`: pre-write `avatar/avatar.toml` with `"x"` → `Ok(AlreadyPresent)` and the file still reads `"x"`.
  - `install_archive_rejects_bad_hash`: wrong hash → `Err`; `avatar/` does not exist afterwards.
  - `install_archive_rejects_missing_referenced_file`: config names `sprite/idle.webp` but the archive lacks it → `Err`; no `avatar/avatar.toml`.
  - `install_archive_rejects_invalid_config`: `default_pose = "nope"` → `Err`.
  - `install_refuses_to_overwrite_existing_file`: pre-create empty `avatar/` with a user file `avatar/sprite/idle.webp` = `"mine"` → `Err`; that file still reads `"mine"`; no `avatar/avatar.toml`; no other files from the archive remain.
  - `install_into_existing_empty_dir`: pre-create empty `avatar/` → `Ok(Installed)`.
  - `staging_dir_is_removed`: after one success and one bad-hash failure, `staging_root` has no entries.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server default_avatar` — new tests fail to compile.
- [ ] **Step 3: Implement** `Outcome` and `install_archive` per the flow; fix the spec wording.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server default_avatar` — all pass.
- [ ] **Step 5: Commit** `feat(workspace): verify and place a default avatar archive (#340)`.

### Task 3: Download and `install`

**Files:**
- Modify: `crates/sapphire-agent-server/src/default_avatar.rs`

**Interfaces:**
- Consumes: `install_archive`, `Outcome`.
- Produces (what #340 task 2 and the future shop call):
  ```rust
  pub(crate) struct AvatarRelease<'a> { pub id: &'a str, pub version: u32, pub url: &'a str, pub sha256: &'a str }
  pub(crate) const INITIAL: AvatarRelease<'static>; // id "default-chibi", version 1,
      // url ".../sapphire-agent-avatars/releases/download/default-chibi-v1/default-chibi-v1.tar",
      // sha256: all-zero placeholder + a `// FIXME(#340): the v1 release's .sha256` comment
  pub(crate) fn install(workspace_root: &Path, release: &AvatarRelease<'_>,
      progress: impl FnMut(u64, Option<u64>)) -> anyhow::Result<Outcome>;
  fn fetch(url: &str, dest: &Path, max: u64, progress: impl FnMut(u64, Option<u64>)) -> anyhow::Result<()>;
  fn staging_root() -> PathBuf; // dirs::cache_dir()/sapphire-agent/avatar-staging, fallback std::env::temp_dir()
  const MAX_DOWNLOAD: u64 = MAX_AVATAR_TOTAL + 8 * 1024 * 1024; // tar headers and padding
  ```
- `INITIAL`'s id is the spec's open item; `default-chibi` is a placeholder to be confirmed at the first release.

`fetch`: `reqwest::blocking::Client` with `connect_timeout(30s)` and no overall timeout; `error_for_status`; pass `content_length()` as `total`; copy in 64 KiB chunks into `dest`, calling `progress(received, total)` after each; fail (and remove `dest`) once `received > max` or if `total > max` up front.

`install`: if `avatar/avatar.toml` exists → `AlreadyPresent` without touching the network; else create `staging_root()`, `fetch` into `<staging_root>/<id>-v<version>-<pid>.tar`, `install_archive(…, staging_root())`, and remove the downloaded tar whatever the result.

- [ ] **Step 1: Write the failing tests** with a loopback server: `fn serve_once(body: Vec<u8>, content_length: Option<usize>) -> String` spawns a thread on `TcpListener::bind("127.0.0.1:0")`, answers one request with `200` (and `Content-Length` when given) and returns the `http://127.0.0.1:<port>/a.tar` URL.
  - `fetch_writes_body_and_reports_progress`: 200 KiB body → file bytes equal; the last progress call is `(204800, Some(204800))`.
  - `fetch_aborts_past_cap`: `max = 1000`, 5000-byte body with no `Content-Length` → `Err`; `dest` does not exist.
  - `fetch_rejects_declared_size_over_cap`: `Content-Length: 5000`, `max = 1000` → `Err` before reading the body.
  - `install_end_to_end`: serve a valid archive, `AvatarRelease` with its hash → `Ok(Installed)`.
  - `install_skips_without_network_when_present`: pre-written `avatar/avatar.toml`, URL `http://127.0.0.1:9/` → `Ok(AlreadyPresent)`.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server default_avatar` — fail to compile.
- [ ] **Step 3: Implement** `AvatarRelease`, `INITIAL`, `fetch`, `staging_root`, `install`.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server default_avatar`, then `cargo clippy -p sapphire-agent-server --all-targets -- -D warnings` and `cargo fmt --check` — all clean.
- [ ] **Step 5: Commit** `feat(workspace): download and install the default avatar (#340)`.
