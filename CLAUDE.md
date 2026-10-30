# Repository conventions for AI assistants

Loaded by Claude Code at session start. Keep it short and operational; deep design notes belong in `docs/` or per-crate READMEs.

## Conventional commit scopes

This workspace ships six crates and uses **conventional-commit scopes** to route changelog entries (and only changelog entries — see "Misattribution" below).

| Scope | Targets |
|---|---|
| `(desktop)` | `desktop/` (bevy GUI client, which runs the CLI too) |
| `(gui)` | `crates/sapphire-agent-gui/` (the bevy + egui GUI shared by the desktop and a future mobile app) |
| `(rpc)` | `crates/sapphire-agent-core/` (local IPC chat protocol + client, shared by `chat` and the desktop) |
| `(core)` | retired with `crates/sapphire-agent-client/` (#278); still skipped by `cliff.toml` |
| unscoped, or `(agent)` / `(messages)` / `(voice)` / `(serve)` / `(channel)` / `(matrix)` / `(discord)` / `(sessions)` / `(chat)` / `(timer)` / `(heartbeat)` / `(memory)` / `(image-cache)` / `(api)` / `(tools)` / `(search)` / `(fts)` / `(mcp)` / `(features)` / `(workspace)` / `(deps)` | the agent: `crates/sapphire-agent-server/`, `crates/sapphire-agent-cli/` and `headless/`; `(cli)` / `(call)` / `(headless)` too |
| `(release)`, `(release-plz)`, `(fmt)`, `(ci)`, `(test)` | infrastructure — workspace-wide, no semver impact intended |

Use the scope that names the crate or sub-area you're changing. The release-plz changelog filter (`cliff.toml`) keys off this.

## Workspace layout: directory ↔ package

The repo root is a **virtual workspace manifest** (`[workspace]` only, no `[package]`). Binaries live in role-named top-level directories; libraries stay in `crates/`. Directory names are shorter than the package names for readability — release-plz keys off the package `name` in each `Cargo.toml`, not the directory, so the two need not match.

| Directory | Package | Kind |
|---|---|---|
| `headless/` | `sapphire-agent-headless` | bin (`sapphire-agent`) — the headless edition: the CLI and nothing else |
| `desktop/` | `sapphire-agent-desktop` | bin (`sapphire-agent-desktop`) — the window, or the CLI given a subcommand |
| `crates/sapphire-agent-cli/` | `sapphire-agent-cli` | lib — the command line both editions run (`run`), `chat` |
| `crates/sapphire-agent-server/` | `sapphire-agent-server` | lib — the app server and the agent (`serve`, plus `init`, `verify`, `acp`) |
| `crates/sapphire-agent-core/` | `sapphire-agent-core` | lib |
| `crates/sapphire-agent-gui/` | `sapphire-agent-gui` | lib (`publish = false`) |

**One app, one binary** (#278): the server and its CLI are the single `sapphire-agent` binary, the way `sapphire-ledger` and the other sapphire-framework apps are. `serve` runs the server (a bare invocation prints the help); the client commands (`chat`, `acp`, and the framework's `status`, `workspace`, …) talk to the server already running on the host over the app server's IPC endpoint — never over HTTP, and they start none. The agent's own IPC methods are versioned by `APP_API` in `crates/sapphire-agent-server/src/serve/acp_tunnel.rs`; bump it when adding or changing one.

**Two editions** (#334, #336): the headless one (`headless/`, binary `sapphire-agent`) and the desktop app (`desktop/`, binary `sapphire-agent-desktop`). Both run the same command line, `sapphire_agent_cli::run`: the desktop's `main` parses it with the subcommand optional and opens the bevy window only when there is none, so the desktop is a superset of the headless edition and both put `sapphire-agent` on PATH (the desktop's installer names it so; its cargo binary name differs because two workspace binaries of one name would collide in `target/`). Install one or the other; `status` warns when the `sapphire-agent` on PATH is another executable. The desktop window is a local IPC client like `chat`, sharing `sapphire-agent-core` with it.

`crates/sapphire-agent-server/` carries the agent's `templates/` and `config.example.toml` (it embeds the templates via `include_str!` and its tests read `config.example.toml` via `CARGO_MANIFEST_DIR`); `headless/` carries the agent's `CHANGELOG.md`. `default-members = ["headless"]` keeps a bare `cargo build` at the root scoped to the agent and away from bevy; run the tests with `--workspace` (or `-p sapphire-agent-server`).

Note the crates.io packages for the libraries retain their pre-rename names until republished: the core lib as `sapphire-agent-rpc` (see the crates.io publish-plan in the tracking issue). The headless edition publishes as `sapphire-agent-headless` from 0.12.0 (up to 0.11.2 as `sapphire-agent`); `sapphire-agent-cli` and `sapphire-agent-server`, binaries up to 0.8.0, resume as its libraries at 0.12.0. A future ESP32-S3 client firmware would live in a separate nested workspace (its own `Cargo.lock` / `rust-toolchain.toml` / `.cargo/config.toml`), added to `[workspace] exclude`, not as a member.

### Two ONNX Runtimes cannot share a binary — keep `ort` out of the default build

`sherpa-onnx-sys` vendors an ONNX Runtime; `ort` vendors another. Both statically link their own copy of **re2**, so putting them in one binary gives duplicate symbols (`re2::Regexp::Concat` and friends) under `rust-lld` on Linux, and LNK2038 on Windows where the CRTs disagree as well. Whether a given link fails depends on which objects the linker happens to pull: the agent's release binary linked fine while its *test* binary did not.

This is not a Windows quirk, and it is the reason `sherpa-onnx` was briefly linked **shared** (66f1c10) — which in turn shipped a binary that could not start anywhere (#182), because the libraries stay in `target/` and nothing writes an RPATH.

The resolution is to keep `ort` out of the default graph, which restores static linking and single-file distribution everywhere:

- **`crates/sapphire-agent-server/`** (the agent) — no longer embeds in-process at all: embedding moved to the sapphire bridge (framework 0.17), and the `fastembed-embed` feature (off by default since #225) is gone.
- The old voice-satellite CLI's openWakeWord detector is gone, and so is the satellite (#278).
- **`desktop/`** — never pulled `ort`. It links the agent (and so sherpa-onnx) now that it runs the CLI, statically like the headless edition.

So `cargo build --workspace` links and tests on Linux, macOS and Windows; CI's `test-platforms (windows-latest)` keeps it that way. Windows is a release target again (#299, reversing #182): since the CLI and the server became one binary (#278) the CLI has to run there, which means the server does too. Code that names paths or spawns processes has to hold up on Windows — show workspace-relative paths with `workspace::slash_path`, not `Path::display`, and gate tests that run POSIX shell scripts to `cfg(unix)`.

### Historical note: agent false-positive release PRs (now fixed)

`sapphire-agent` used to live *at* the workspace root, so **release-plz attributed any workspace-root file change to it** — including `Cargo.lock`, which almost every sibling-crate commit touches. That produced spurious `sapphire-agent` release PRs (investigated in closed [#143](https://github.com/fluo10/sapphire-agent/pull/143), [#144](https://github.com/fluo10/sapphire-agent/pull/144)). release-plz's release-trigger logic is purely path-based and no config (regex filter, git-cliff `skip = true`, etc.) suppressed it. Moving the package into a subdirectory (`server/`, then `cli/`, now `headless/` and `crates/`) — leaving no package at the root — is the structural fix: root `Cargo.lock` churn is no longer inside any package's directory.

## Extending the changelog filter

`cliff.toml` skips commits whose scope is in this list:

```
desktop | rpc | core | gui | release | release-plz | fmt | ci | test | deps
```

- **Sibling crate scope, new addition**: extend the alternation in `cliff.toml` → `[[git.commit_parsers]] message = '^[a-z]+\\((…)\\):'` at the same time as the first commit using it. Otherwise the first such commit will pollute the agent changelog.
- **New agent-internal sub-area** (e.g. you introduce `feat(notifications):`): **no `cliff.toml` change needed**. Agent-internal scopes fall through to the conventional-type parsers (`feat` → "added", `fix` → "fixed", etc.) and show up in the agent changelog as expected.

Rust's `regex` crate has no negative lookahead, so the filter is an allowlist of "scopes to drop" rather than "scopes to keep". This is why new agent-internal scopes don't need re-listing — only new sibling/infra scopes do.

## Release flow recap

- `release-plz` creates the tags and GitHub releases on push to main. `sapphire-agent-headless`, `sapphire-agent-cli`, `sapphire-agent-server` and `sapphire-agent-desktop` share one version (`version_group = "app"`) and one `v{version}` tag and release, which the headless edition makes; `sapphire-agent-core` keeps its own version and gets none (the sapphire-turtle convention; `desktop-v*` and per-package `sapphire-agent-*-v*` tags are history).
- `.github/workflows/release-plz.yml` calls sapphire-turtle/.github's shared `release-plz.yml` and, once the headless edition is released, `release-binaries.yml`, which builds both binaries through the shared `release-binary.yml` and attaches them to the `v{version}` release as `sapphire-agent-headless-<os>-<arch>` and `sapphire-agent-desktop-<os>-<arch>`. CI is the shared `rust-ci.yml`; the sherpa-onnx cache refresh and the self-contained check live in `.github/scripts/`.
- Tags a workflow pushes don't fire downstream workflows on their own — the `workflow_call` chain inside `release-plz.yml` is the path; running `release-binaries.yml` by hand (`-f tag=v…`) is the retroactive escape hatch.
- **A GUI-only change makes no desktop release.** release-plz picks packages by path, and `sapphire-agent-gui` has `release = false` while `(gui)` is skipped by `cliff.toml`, so a commit touching only `crates/sapphire-agent-gui/` produces no release. A user-visible GUI change must also touch `desktop/` in the same PR (a `(desktop)` commit, its CHANGELOG or version), or the desktop will not ship it.
- `sapphire-agent-desktop` carries `publish = false`; release-plz still versions it because `release-plz.toml` sets `git_only = true` (it skips a `publish = false` crate without it; `release = true` alone no longer does it), reading its version from the shared `v{version}` tags with its own tag and release turned off.
