# Design: memory tools across included namespaces

Date: 2026-11-24

Companion to `2026-11-24-session-profile-prompt-files-design.md`; same
deployment (operator, a family member doing confidential translation work,
website guests).

## Intent

Facts about the family belong in one shared `family` namespace that both the
operator's and the family member's namespaces `include`, written from either
side and read from both. Today `include` only chains MEMORY.md into the
system prompt; the `memory_*` tools reach the session's own namespace alone,
so shared facts cannot be written or read where they belong.

## Goals / non-goals

- Goal: the `memory_*` tools read and write any namespace in the session's
  include chain, defaulting to its own.
- Goal: the agent can tell namespaces apart by an operator-written
  description, not just a name.
- Goal (found during design): `workspace_search` stops leaking other
  profiles' file paths to sessions that have no file access.
- Non-goal: background writers (daily log, digests, MEMORY.md compaction)
  keep writing to the session's own namespace only.
- Anti-goal: any reach outside the include chain.

## Approaches considered

1. **Optional `namespace` argument on the existing tools — chosen.** One
   tool set, default unchanged, allowed set checked per call.
2. Encoding the namespace in `category` (`family/knowledge`). Rejected:
   overloads a validated path segment and breaks existing conventions.
3. Separate tools per namespace. Rejected: tool specs are static (and part
   of the cached prompt prefix); per-session tool sets would churn them.

For `workspace_search`: moving it into the `fs` group was chosen over
dropping it (the operator keeps search) and over namespace-filtering its
results (an `fs` session can already read the whole workspace, so a filter
would protect nothing there, and `AGENTS.<p>.md`-style files outside
`memory/` would still leak).

## Design

- **Config**: `[memory_namespace.<n>].description: Option<String>`. The
  implicit `default` can be described by writing `[memory_namespace.default]`.
- **Tools**: `memory_add`, `memory_update`, `memory_append`, `memory_read`,
  `memory_remove` take an optional `namespace` string. Absent → the
  session's own namespace (today's behaviour). Present → must be in
  `Config::resolve_namespace_chain(own)` (own namespace plus everything
  reachable through `include`, transitively), else the call fails with an
  error naming the allowed set. Read and write share the same allowed set:
  `include` is operator-configured, so "may see" equals "may write".
  The chain is computed from the namespace in `current_memory_namespace()`
  (the task-local already scoped per turn and inherited by subagents); the
  tools need access to `Config` for this.
- **Prompt**: the `# Memory` block opens with a list of the chain's
  namespaces — name, a marker on the session's own, and its description —
  including namespaces with no MEMORY.md, so a fresh `family` is known as a
  write target. The list is fixed per session profile, so it does not
  disturb prompt caching. The tools' `namespace` parameter description says
  to pick from that list and to default to the own namespace.
- **`workspace_search` / `workspace_sync`**: move to `ToolGroup::Fs` (register
  under `group_registered(ToolGroup::Fs)`, update the tool→group table in
  `tools/mod.rs` and the tests that pin it). A `chat`-permission session
  then has no workspace-wide tool at all.

## Constraints

- Docs (config.example.toml, README) state that `fs` reaches the whole
  workspace regardless of namespace, so profiles that must not see other
  profiles' data — guests in particular — use the `chat` permission.
- Workspace-relative paths in tool reports go through `workspace::slash_path`.

## Left to the builder (my calls, correctable)

- Validate `namespace` with the same rules as `category` / `slug` before the
  chain check.
- Tool result messages name the namespace written to
  (`Wrote memory/family/knowledge/…`), so the operator can see where a fact
  landed.
- CHANGELOG: moving `workspace_search` into `fs` is a behaviour change for
  any profile without `fs`; call it out.
- Tests: default namespace, included namespace read/write, transitive
  include, out-of-chain refusal, namespace list in prompt (with and without
  MEMORY.md, with and without description), `workspace_search` absent
  without `fs`.
