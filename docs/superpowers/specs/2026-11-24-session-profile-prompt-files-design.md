# Design: per-session-profile workspace prompt files

Date: 2026-11-24

## Intent

One agent serves three kinds of people, each through its own session profile:
the operator (daily life and development), a family member doing confidential
translation work, and guests arriving from the website's contact form. Memory
and sessions are already separated by namespace, but the role-defining files
(`AGENTS.md`, `SOUL.md`, …) are read once from the workspace root and are the
same for everyone. Each session profile needs its own role, layered over a
small shared core.

## Goals / non-goals

- Goal: a session profile can contribute its own version of every workspace
  prompt file, merged after the root file by default or replacing it.
- Goal: zero config for the common case (a naming convention), with an
  explicit path override.
- Non-goal: tying prompt files to memory namespaces or their `include` chain.
  Memory `include` decides which *knowledge* is visible; it must not drag a
  role along with it.
- Non-goal: access control. Prompt files shape behaviour only; keeping one
  profile's memory away from another is the permission profile's job (see
  Constraints).
- Anti-goal: a guest prompt that silently carries the operator's personal
  context (e.g. a root `USER.md`) — replacing must be able to drop the root
  file entirely.

## Approaches considered

1. **Per memory namespace, root + leaf** (`memory/<ns>/AGENTS.md`). Rejected:
   a prompt file inside `memory/` is unintuitive, and not every namespace
   needs a role.
2. **Per memory namespace, following the include chain.** Rejected: sharing
   memory would also inherit a role (e.g. the operator including the family
   namespace would pick up translation instructions).
3. **Per session profile, root + that profile only — chosen.** The session
   profile is already the unit that bundles "who is this conversation for"
   (LLM profile, namespace, permission), so the role belongs there.

Merge default: **append** (root first, profile after), chosen over
replace-by-default so the shared core is written once and every profile picks
up changes to it.

## Design

Applies to every entry in `WORKSPACE_FILES` (AGENTS, SOUL, IDENTITY, USER,
TOOLS, BOOTSTRAP). For session profile `<p>` and a file whose root candidates
are e.g. `AGENTS.md` / `AGENT.md`:

1. **Profile file resolution**
   - If `[session_profile.<p>].<kind>_file` is set (`agents_file`,
     `soul_file`, `identity_file`, `user_file`, `tools_file`,
     `bootstrap_file`), that workspace-relative path is the profile file.
   - Otherwise the first existing of `AGENTS.<p>.md`, `AGENT.<p>.md` in the
     workspace root.
2. **Root file**: today's behaviour, unchanged (`AGENTS.md`, then `AGENT.md`).
3. **Merge**: the profile file may start with a YAML frontmatter block
   (parsed with `crate::frontmatter`) carrying `merge: append | replace`;
   absent means `append`. The frontmatter is stripped before injection.
   - `append`: one section, the existing heading (`# Agent Instructions`),
     root body, a blank line, profile body.
   - `replace`: the profile body alone. A `replace` file with an empty body
     drops the file for that profile (this is how "none" is expressed).
   - No profile file: the root file alone, as today.
   - Section is omitted when the result is empty.
4. **No session profile** (e.g. an RPC session with no pin): root files only.
5. `build_system_prompt` takes the session profile name (or the resolved
   `SessionProfileConfig`) in addition to its current arguments; both
   callers (`agent.rs` snapshot rebuild, `serve/mod.rs` RPC turn) already
   know it. Reads go through the existing pinned cache and the
   `MAX_FILE_CHARS` per-file truncation.

Intended layout for this deployment (illustrative, not shipped):

```
AGENTS.md              # minimal shared core
AGENTS.admin.md        # daily life + development
AGENTS.family.md       # translation support, confidentiality rules
AGENTS.guest.md        # contact-form front desk
USER.admin.md / USER.family.md   # no root USER.md
SOUL.guest.md          # merge: replace
```

## Constraints

- Per the guest anti-goal, documentation (config.example.toml, README) must
  say plainly that per-profile prompts are not isolation: a guest profile
  should use the `chat` permission profile (no tools), since the built-in
  `default` grants `fs` and could read other namespaces' memory files.
- Paths shown to the model or in reports go through `workspace::slash_path`.

## Left to the builder (my calls, correctable)

- An explicitly configured `<kind>_file` that does not exist: warn once at
  startup and treat the profile file as absent (no fallback to
  `AGENTS.<p>.md`, so a typo is noticed rather than masked).
- An unknown `merge:` value: warn and treat as `append`.
- `verify`'s workspace-file listing (`lib.rs`, `workspace_files`) gains a
  per-session-profile line showing which file it resolved and its merge mode.
- Profile names are used verbatim in file names; names containing `/` or
  `\` are rejected by config validation for this purpose (or simply never
  match a file — builder's choice, tested either way).
- Tests: append, replace, empty replace, explicit path, missing explicit
  path, no profile, `AGENT.<p>.md` fallback, frontmatter stripped.
