# Per-profile prompt files and include-chain memory access — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let each session profile layer its own `AGENTS.md`/`SOUL.md`/… over the root files, let the memory tools read and write every namespace in the session's include chain, and stop `workspace_search` from reaching sessions without file access.

**Architecture:** Prompt-file resolution lives in `workspace.rs` and takes the session profile name plus its config; `build_system_prompt` grows one parameter. Memory scoping changes its task-local from a single namespace to the resolved chain (own namespace first), so the tools check membership without needing `Config`. `workspace_search`/`workspace_sync` move into `ToolGroup::Fs`.

**Tech Stack:** Rust, tokio, serde; `crate::frontmatter` for YAML frontmatter.

**Specs:**
- `docs/superpowers/specs/2026-11-24-session-profile-prompt-files-design.md`
- `docs/superpowers/specs/2026-11-24-memory-namespace-include-access-design.md`

All paths below are relative to `crates/sapphire-agent-server/` unless they start with `docs/` or `headless/`.

## Global Constraints

- Commit scopes: unscoped or agent-internal (`(agent)`, `(memory)`, `(tools)`, `(workspace)`) — never a sibling-crate scope; these land in the agent changelog.
- Default merge mode is `append`; `merge: replace` in the profile file's frontmatter replaces; an empty-body `replace` drops the file for that profile.
- Profile file lookup order: `[session_profile.<p>].<kind>_file` → `<STEM>.<p>.md` (each root candidate, e.g. `AGENTS.<p>.md` then `AGENT.<p>.md`) → none. Root lookup unchanged.
- Config keys: `agents_file`, `soul_file`, `identity_file`, `user_file`, `tools_file`, `bootstrap_file` on `SessionProfileConfig`; `description` on `MemoryNamespaceConfig`.
- Memory tools: optional `namespace` argument; allowed set is `Config::resolve_namespace_chain(own)`; read and write share it; default is own.
- Background writers (daily log, digests, compaction) are untouched.
- Workspace-relative paths shown to the model go through `workspace::slash_path`.
- The prompt must stay byte-identical across turns for a fixed profile (prompt cache).
- Run tests with `cargo test -p sapphire-agent-server`; full check `cargo test --workspace` before the last commit.

## Review Focus

1. A profile name containing `/`, `\`, or starting with `.` must never be used to build a file name (path traversal via config) — it simply gets no convention file; explicit `*_file` still works. Test in Task 1.
2. A profile file with frontmatter but no `merge:` key (e.g. other metadata) is `append`, and its frontmatter is still stripped. Test in Task 1.
3. A profile file that is whitespace-only without frontmatter must not add an empty trailing section in `append` mode (root alone). Test in Task 1.
4. `namespace` given as `""` or `"../default"` is refused by segment validation before the chain check. Test in Task 3.
5. A subagent delegated from a `family`-included session can write to `family` too (it inherits the chain, not just the own namespace). Test in Task 3.

---

### Task 1: Prompt-file resolution per session profile

**Files:**
- Modify: `src/config.rs` (`SessionProfileConfig`, ~line 264)
- Modify: `src/workspace.rs` (`WorkspaceFileDef`, `WORKSPACE_FILES`, `build_system_prompt`, tests)

**Interfaces:**
- Produces:
  - `config::PromptFileKind` enum `{ Agents, Soul, Identity, User, Tools, Bootstrap }` (Copy, Debug, PartialEq).
  - `SessionProfileConfig::prompt_file(&self, kind: PromptFileKind) -> Option<&str>` returning the matching `*_file` field.
  - Six `#[serde(default)] pub <kind>_file: Option<String>` fields on `SessionProfileConfig`.
  - `workspace::PromptProfile<'a> { pub name: &'a str, pub config: &'a SessionProfileConfig }` (Copy).
  - `Workspace::build_system_prompt(&self, base, boundary_hour, namespace_chain, room_info, cwd, profile: Option<PromptProfile<'_>>) -> String` — new last parameter.
  - `workspace::profile_file_candidates(kind_root_candidates: &[&str], profile_name: &str) -> Vec<String>` (pub(crate)): `["AGENTS.md","AGENT.md"]`, `"family"` → `["AGENTS.family.md","AGENT.family.md"]`; empty when the name is empty, contains `/` or `\` or `\0`, or starts with `.`.
  - `workspace::MergeMode { Append, Replace }` and `fn split_merge_mode(raw: &str) -> (MergeMode, String)` — strips frontmatter via `crate::frontmatter::split`/`parse_mapping`; missing key → `Append`; unknown value → `tracing::warn!` + `Append`.

- [ ] **Step 1: Write failing tests in `workspace.rs` `mod tests`**

Helper: `fn profile(name: &str, cfg: &SessionProfileConfig) -> Option<PromptProfile<'_>>`. Each test writes files into a `TempDir`, builds `Workspace::new(dir, DigestConfig::default())`, calls `build_system_prompt(None, 4, &["default".into()], None, None, profile)`.

```rust
// root AGENTS.md "core", AGENTS.family.md "translate"
fn profile_file_is_appended_after_root()        // contains "# Agent Instructions\n\ncore\n\ntranslate"; exactly one "# Agent Instructions"
// AGENTS.family.md = "---\nmerge: replace\n---\ntranslate"
fn replace_drops_root_and_strips_frontmatter() // contains "translate"; !contains "core"; !contains "merge:"
// SOUL.md "root soul", SOUL.guest.md = "---\nmerge: replace\n---\n"
fn empty_replace_omits_the_section()           // !contains "# Soul"
// agents_file = Some("roles/x.md") with "explicit"; AGENTS.family.md "convention"
fn explicit_path_wins_over_convention()        // contains "explicit"; !contains "convention"
// agents_file = Some("missing.md"); AGENTS.family.md "convention"; root "core"
fn missing_explicit_path_does_not_fall_back()  // contains "core"; !contains "convention"
fn no_profile_uses_root_only()                 // profile None, AGENTS.family.md present → !contains its text
// only AGENT.family.md exists (no AGENTS.family.md)
fn singular_agent_profile_file_is_found()
// profile name "../etc" and ".hidden": profile_file_candidates returns empty
fn unsafe_profile_names_get_no_convention_file()
// AGENTS.family.md = "---\ntitle: x\n---\nbody"
fn frontmatter_without_merge_key_appends()     // contains "core\n\nbody"; !contains "title:"
// AGENTS.family.md = "   \n"
fn whitespace_profile_file_adds_nothing()      // section equals root-only output
// call twice with same profile → identical strings
fn profile_prompt_is_byte_identical_across_turns()
```

- [ ] **Step 2: Run them — expect compile failure (missing `PromptProfile`, extra arg)**

Run: `cargo test -p sapphire-agent-server workspace::tests`

- [ ] **Step 3: Implement**

Add `kind: PromptFileKind` to `WorkspaceFileDef`. In the loop over `WORKSPACE_FILES`, compute `root = read_first_existing(def.candidates)` (body unchanged, as today) and `prof`: explicit path via `read_file(path)` if `prompt_file(kind)` is `Some`, else first existing of `profile_file_candidates`. Combine per Global Constraints; push `format!("{heading}\n\n{body}")` only for a non-blank body. Update the existing tests' calls in this file to pass `None`.

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo test -p sapphire-agent-server workspace::tests`

- [ ] **Step 5: Commit** — `feat(workspace): per-session-profile prompt files merged over the root ones`

### Task 2: Wire the profile into prompt callers, verify, startup warning, docs

**Files:**
- Modify: `src/agent.rs:~470` (`get_system_prompt` — has `profile_name`)
- Modify: `src/serve/mod.rs:~3354` (has `session_profile`)
- Modify: `src/cli_init.rs:374` (pass `None`)
- Modify: `src/config.rs` — add `Config::missing_prompt_file_overrides(&self, workspace_dir: &Path) -> Vec<String>`
- Modify: `src/lib.rs:~443` (serve startup: `warn!` each entry) and `src/lib.rs:~1674` (verify listing)
- Modify: `config.example.toml` (session_profile section ~line 299), `README.md` of the server crate if it documents workspace files
- Modify: `headless/CHANGELOG.md` only if the project hand-edits it (check `git log headless/CHANGELOG.md`; release-plz usually writes it — then skip)

**Interfaces:**
- Consumes: `PromptProfile`, `PromptFileKind`, `SessionProfileConfig::prompt_file` (Task 1).
- Produces: `missing_prompt_file_overrides` returns lines `"session_profile '<p>' <kind>_file '<path>' does not exist"`, sorted by profile then kind.

- [ ] **Step 1: Failing test in `config.rs` tests** — `missing_prompt_file_overrides_names_only_absent_paths`: profile `family` with `agents_file = "a.md"` (exists in TempDir) and `soul_file = "s.md"` (absent) → exactly `["session_profile 'family' soul_file 's.md' does not exist"]`.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server missing_prompt_file_overrides` — FAIL.
- [ ] **Step 3: Implement** the method; pass `config.session_profile(name).map(|c| PromptProfile { name, config: c })` at the two callers (serve: `None` when the profile is not configured); startup warns; verify prints, under the existing workspace-file list, one line per session profile and kind that resolves a profile file: `  <p>: AGENTS.family.md (append)`.
- [ ] **Step 4: Docs** — in `config.example.toml` add commented `agents_file`/`soul_file` examples under a session profile, the naming convention, the `merge: replace` frontmatter, and the warning: per-profile prompts are not isolation; a guest profile should use `permission = "chat"` because `fs` reaches the whole workspace.
- [ ] **Step 5: Run** `cargo test -p sapphire-agent-server` — PASS (existing serve test at `serve/mod.rs:~6317` still sees the workspace SOUL.md).
- [ ] **Step 6: Commit** — `feat(agent): load session-profile prompt files in chat and RPC turns`

### Task 3: Memory tools across the include chain

**Files:**
- Modify: `src/config.rs` (`MemoryNamespaceConfig.description: Option<String>`, `#[serde(default)]`)
- Modify: `src/tools/workspace_tools.rs` (task-local, `memory_entry_path`, five tools' schemas and `execute`)
- Modify: call sites of `scope_memory_namespace`: `src/agent.rs:~901`, `src/serve/mod.rs:~2998`, `src/tools/subagent.rs:~891,~1001,~1439`, test at `src/tools/session_tools.rs:~680`

**Interfaces:**
- Produces:
  - `scope_memory_namespace(chain: Vec<String>, fut)` — signature change; `chain[0]` is the own namespace; callers pass `config.resolve_namespace_chain(&ns)`.
  - `current_memory_namespace() -> String` unchanged semantics (first chain entry, `"default"` outside scope).
  - `pub(crate) fn current_memory_chain() -> Vec<String>` (`vec!["default"]` outside scope). Subagent paths pass this instead of a bare namespace.
  - `memory_entry_path(state, namespace: Option<&str>, category, slug) -> Result<(String, PathBuf)>`: validates `namespace` with `validate_segment("namespace", …)`, then requires membership in `current_memory_chain()`, else `bail!("namespace '{ns}' is not available here; choose one of: {list}")`.
  - Schema: `"namespace": {"type":"string","description":"Memory namespace to use, from the list under # Memory. Omit for this conversation's own namespace."}` (not required).
  - Tool reports keep `rel` (which already contains `memory/<ns>/…`).

- [ ] **Step 1: Failing tests in `workspace_tools.rs` tests** (scope with `scope_memory_namespace(vec!["mom".into(), "family".into()], …)`):
  - `memory_add_defaults_to_own_namespace` → file at `memory/mom/knowledge/x.md`.
  - `memory_add_writes_to_included_namespace` → `namespace:"family"` → `memory/family/knowledge/x.md`; result contains `memory/family/`.
  - `memory_read_reads_included_namespace`.
  - `out_of_chain_namespace_is_refused` → `namespace:"admin"` errors, message lists `mom, family`; no file created.
  - `invalid_namespace_segment_is_refused` → `""` and `"../default"` error.
  - `transitive_include_is_reachable` — config-level: `mom → family → household`; `resolve_namespace_chain("mom")` passed to scope; write to `household` succeeds.
  - In `subagent.rs` tests (follow the existing background-scope test pattern near line 1439): `background_subagent_inherits_memory_chain` — `current_memory_chain()` inside the scoped future equals the parent's chain.
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server workspace_tools` — FAIL (signature).
- [ ] **Step 3: Implement**; change task-local type to `Vec<String>`; update all call sites listed above.
- [ ] **Step 4: Run** `cargo test -p sapphire-agent-server` — PASS.
- [ ] **Step 5: Commit** — `feat(memory): memory tools reach namespaces in the include chain`

### Task 4: Namespace list in the `# Memory` prompt block

**Files:**
- Modify: `src/workspace.rs` (`build_memory_block`, `build_system_prompt` signature)
- Modify: callers from Task 2 (`agent.rs`, `serve/mod.rs`, `cli_init.rs`)

**Interfaces:**
- Consumes: `MemoryNamespaceConfig.description` (Task 3).
- Produces: `build_system_prompt`'s `namespace_chain: &[String]` parameter becomes `namespaces: &[NamespaceInfo]` (same order; `namespaces[0]` is the own namespace), where `pub struct NamespaceInfo { pub name: String, pub description: Option<String> }` lives in `config.rs`, built by `Config::namespace_infos(&self, chain: &[String]) -> Vec<NamespaceInfo>`. Callers pass `config.namespace_infos(&config.resolve_namespace_chain(ns))`; internal uses of the chain (MEMORY.md, periodic logs) read `.name`.
- Block format (exact):

```
# Memory

Namespaces available to the memory tools (pass `namespace` to choose; default is this conversation's own):
- mom (this conversation) — 母の翻訳業務
- family — 家族に関する共有情報

## mom

<MEMORY.md>
```

A namespace without a description renders as `- name` / `- name (this conversation)`. The list is emitted even when no MEMORY.md exists in the chain.

- [ ] **Step 1: Failing tests in `workspace.rs`**:
  - `memory_block_lists_namespaces_with_descriptions` — exact list lines above.
  - `memory_block_lists_namespaces_without_memory_md` — no MEMORY.md anywhere → block present with list, no `## ` subsections.
  - `namespace_without_description_has_bare_name`.
  - Existing MEMORY.md chaining tests keep passing (subsections unchanged after the list).
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server workspace::tests` — FAIL.
- [ ] **Step 3: Implement**; also add `Config::namespace_infos` with a test `namespace_infos_reads_descriptions_and_defaults_missing`.
- [ ] **Step 4: Docs** — `config.example.toml` `[memory_namespace.*]` examples gain `description`, plus a `family` example included by two namespaces.
- [ ] **Step 5: Run** `cargo test -p sapphire-agent-server` — PASS.
- [ ] **Step 6: Commit** — `feat(memory): describe the include chain's namespaces in the prompt`

### Task 5: `workspace_search` and `workspace_sync` require `fs`

**Files:**
- Modify: `src/tools/workspace_tools.rs` (`group: Some(ToolGroup::Fs)` in both specs)
- Modify: `src/tools/mod.rs:~350` (move both into the `group_registered(ToolGroup::Fs)` block) and the kind/group table test (`~536`, `~689`)
- Modify: `src/agent.rs:~1066,~1103,~1144` tests that expect `workspace_sync` in a tool set — adjust to the permission profile those tests use
- Modify: memory tools' descriptions that say "retrievable via workspace_search" → keep, but add "(when file tools are available)"
- Modify: `config.example.toml` permission section — note search needs `fs`

- [ ] **Step 1: Update the table test** to `("workspace_search", ToolKind::Search, Some(ToolGroup::Fs))` and add `("workspace_sync", ToolKind::Other, Some(ToolGroup::Fs))`; add `chat_permission_has_no_workspace_search` asserting the `chat` permission's tool list lacks both (follow the existing per-permission filtering test near `tools/mod.rs:577`).
- [ ] **Step 2: Run** `cargo test -p sapphire-agent-server tools::` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — PASS; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean.
- [ ] **Step 5: Commit** — `fix(tools): workspace_search and workspace_sync need the fs group` (no `!`: a 0.x breaking marker would force a minor bump; the user decides) with a body noting the behaviour change for profiles without `fs`.
