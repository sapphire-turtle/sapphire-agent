# Repository conventions for AI assistants

Loaded by Claude Code at session start. Keep it short and operational; deep design notes belong in `docs/` or per-crate READMEs.

## Conventional commit scopes

This workspace ships four crates and uses **conventional-commit scopes** to route changelog entries (and only changelog entries — see "Misattribution" below).

| Scope | Targets |
|---|---|
| `(desktop)` | `desktop/` (bevy GUI client) |
| `(rpc)` | `crates/sapphire-agent-core/` (RPC client library) |
| `(core)` | `crates/sapphire-agent-client/` (shared config + device_id) |
| unscoped, or `(agent)` / `(messages)` / `(voice)` / `(serve)` / `(channel)` / `(matrix)` / `(discord)` / `(sessions)` / `(chat)` / `(timer)` / `(heartbeat)` / `(memory)` / `(image-cache)` / `(api)` / `(tools)` / `(search)` / `(fts)` / `(mcp)` / `(features)` / `(workspace)` / `(deps)` | the agent binary (`cli/`); `(cli)` / `(call)` too |
| `(release)`, `(release-plz)`, `(fmt)`, `(ci)`, `(test)` | infrastructure — workspace-wide, no semver impact intended |

Use the scope that names the crate or sub-area you're changing. The release-plz changelog filter (`cliff.toml`) keys off this.

## Workspace layout: directory ↔ package

The repo root is a **virtual workspace manifest** (`[workspace]` only, no `[package]`). Binaries live in role-named top-level directories; libraries stay in `crates/`. Directory names are shorter than the package names for readability — release-plz keys off the package `name` in each `Cargo.toml`, not the directory, so the two need not match.

| Directory | Package | Kind |
|---|---|---|
| `cli/` | `sapphire-agent` | bin (`sapphire-agent`) — the server and its CLI |
| `desktop/` | `sapphire-agent-desktop` | bin (`sapphire-agent-desktop`) |
| `crates/sapphire-agent-core/` | `sapphire-agent-core` | lib |
| `crates/sapphire-agent-client/` | `sapphire-agent-client` | lib |

**One app, one binary** (#278): the server and its CLI are the single `sapphire-agent` binary in `cli/`, the way `sapphire-ledger` and the other sapphire-framework apps are. The directory is `cli/` because the CLI is what people face; serving is one of its modes. A bare invocation (or `serve`) runs the server; the client commands (`chat`, `acp`, and the framework's `status`, `workspace`, …) talk to the server already running on the host over the app server's IPC endpoint — never over HTTP, and they start none. The agent's own IPC methods are versioned by `APP_API` in `cli/src/serve/acp_tunnel.rs`; bump it when adding or changing one.

`cli/` carries the agent's `templates/`, `config.example.toml`, and `CHANGELOG.md` (the binary embeds the templates via `include_str!` and reads `config.example.toml` via `CARGO_MANIFEST_DIR`). `default-members = ["cli"]` keeps a bare `cargo build` / `cargo test` at the root scoped to the agent.

Note the crates.io packages for the libraries retain their pre-rename names until republished: the RPC lib as `sapphire-agent-rpc`, the client-core lib as `sapphire-call-core` (see the crates.io publish-plan in the tracking issue). The agent binary publishes as `sapphire-agent`, which is its package name again. A future ESP32-S3 client firmware would live in a separate nested workspace (its own `Cargo.lock` / `rust-toolchain.toml` / `.cargo/config.toml`), added to `[workspace] exclude`, not as a member.

### Two ONNX Runtimes cannot share a binary — keep `ort` out of the default build

`sherpa-onnx-sys` vendors an ONNX Runtime; `ort` vendors another. Both statically link their own copy of **re2**, so putting them in one binary gives duplicate symbols (`re2::Regexp::Concat` and friends) under `rust-lld` on Linux, and LNK2038 on Windows where the CRTs disagree as well. Whether a given link fails depends on which objects the linker happens to pull: the agent's release binary linked fine while its *test* binary did not.

This is not a Windows quirk, and it is the reason `sherpa-onnx` was briefly linked **shared** (66f1c10) — which in turn shipped a binary that could not start anywhere (#182), because the libraries stay in `target/` and nothing writes an RPATH.

The resolution is to keep `ort` out of the default graph, which restores static linking and single-file distribution everywhere:

- **`cli/`** (the agent) — no longer embeds in-process at all: embedding moved to the sapphire bridge (framework 0.17), and the `fastembed-embed` feature (off by default since #225) is gone.
- The old voice-satellite CLI's openWakeWord detector is gone, and so is the satellite (#278).
- **`desktop/`** — never pulled `ort`; it only followed the workspace's link mode.

So `cargo build --workspace` works on Linux and macOS. It still does **not** link on Windows for `cli/`, which is fine — the agent is a headless server app with no Windows build (#182). Build the desktop client by name there:

```sh
cargo build --package sapphire-agent-desktop
```

### Historical note: agent false-positive release PRs (now fixed)

`sapphire-agent` used to live *at* the workspace root, so **release-plz attributed any workspace-root file change to it** — including `Cargo.lock`, which almost every sibling-crate commit touches. That produced spurious `sapphire-agent` release PRs (investigated in closed [#143](https://github.com/fluo10/sapphire-agent/pull/143), [#144](https://github.com/fluo10/sapphire-agent/pull/144)). release-plz's release-trigger logic is purely path-based and no config (regex filter, git-cliff `skip = true`, etc.) suppressed it. Moving the package into a subdirectory (`server/`, now `cli/`) — leaving no package at the root — is the structural fix: root `Cargo.lock` churn is no longer inside any package's directory.

## Extending the changelog filter

`cliff.toml` skips commits whose scope is in this list:

```
desktop | rpc | core | release | release-plz | fmt | ci | test | deps
```

- **Sibling crate scope, new addition**: extend the alternation in `cliff.toml` → `[[git.commit_parsers]] message = '^[a-z]+\\((…)\\):'` at the same time as the first commit using it. Otherwise the first such commit will pollute the agent changelog.
- **New agent-internal sub-area** (e.g. you introduce `feat(notifications):`): **no `cliff.toml` change needed**. Agent-internal scopes fall through to the conventional-type parsers (`feat` → "added", `fix` → "fixed", etc.) and show up in the agent changelog as expected.

Rust's `regex` crate has no negative lookahead, so the filter is an allowlist of "scopes to drop" rather than "scopes to keep". This is why new agent-internal scopes don't need re-listing — only new sibling/infra scopes do.

## Release flow recap

- `release-plz` creates per-package tags (`sapphire-agent-v*`, `sapphire-agent-desktop-v*`, ...) and GitHub releases on push to main.
- `.github/workflows/release-plz.yml` parses the `releases` output with `jq` and chains into reusable build workflows (`release.yml`, `release-desktop.yml`) which attach platform binaries to each release.
- Tags pushed by `GITHUB_TOKEN` don't fire downstream workflows on their own — the `workflow_call` chain inside `release-plz.yml` is the path; `workflow_dispatch` with `-f tag=…` is the retroactive escape hatch.
- `sapphire-agent-desktop` carries `publish = false`; release-plz still tags + releases it because `release-plz.toml` sets `release = true` explicitly (the default for `publish = false` crates is to skip them).
