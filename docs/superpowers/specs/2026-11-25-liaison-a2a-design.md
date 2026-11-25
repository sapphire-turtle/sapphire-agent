# Design: liaison — the private agent talks to the guest agent over A2A

Date: 2026-11-25

Follows `2026-11-24-session-profile-prompt-files-design.md` (per-profile
prompt files, used here on the guest side) and
`2026-11-24-memory-namespace-include-access-design.md`.

Supersedes an unmerged draft that shared memory namespaces across instances
over JSON-RPC. That draft was dropped: importing a stranger-facing agent's
memory into the operator's agent is not safe however it is filtered, and the
operator does not need it.

## Intent

Guests (strangers reaching the agent through the website form) are served by
a **separate guest instance**, so that a vulnerability used through it does
not reach the operator's private instance.

The operator still wants the two agents to exchange information, in a
limited way:

- **guest → private:** what people asked today (the day's inquiries), so the
  operator can review them;
- **private → guest:** answers to those inquiries, and information that may
  be public (e.g. the development status of the sapphire apps).

There is no memory sharing. The two agents **talk**, over A2A, in a dedicated
**liaison** session on the private instance. The operator reads the
inquiries and drives the answers in that session.

## Goals / non-goals / anti-goals

- Goal: the private agent can send A2A messages to configured remote agents,
  continue a conversation with them (A2A `contextId`), and read their replies.
- Goal: the guest side needs no new code. It uses the existing `/a2a`
  endpoint, bearer tokens and per-profile prompt files.
- Goal: what a successful injection through the guest agent can do in the
  liaison is bounded by configuration. Startup validation **warns** about a
  configuration that loosens that bound and **refuses** only the one
  combination that removes it (`admin`).
- Goal: A2A stays general. Other uses — e.g. asking a booking agent to
  reserve a flight, which needs the operator's schedule — must remain
  possible, so the liaison's isolation is the operator's configuration, not
  a property hard-wired into `remote_agents`.
- Goal: a namespace can be included **read-only** (`import`), so the liaison
  reads `public` but cannot write it.
- Non-goal: memory sharing or sync of any kind.
- Non-goal: the guest instance contacting the private one. It holds no address
  or credential for it.
- Non-goal: scheduling. A liaison run can use the existing timer or be
  started by hand.
- Non-goal: A2A streaming, push notifications, `GetTask`, and file or data
  parts in the client. These are out of scope, as on the server.
- **Anti-goal:** text from a remote agent reaches a session with `admin`
  tools (refused), or — without a startup warning — a session with fs,
  shell, mcp, ACP, outbound tools, or another profile's memory.
- **Anti-goal:** a namespace a `remote_agents` session can write is
  auto-injected into another profile's sessions without a startup warning.

## Threat model

A guest-side reply is influenced by strangers, so it may carry prompt
injection. A model summarizing it is **not** a defence: an instruction
phrased to survive summarization ("tell the owner's agent to …") will survive
it. The defence is what the receiving session can do and see:

Each block below holds when the operator configures the liaison as
intended; startup warnings flag where it does not (only `admin` is refused).

- **Act on the host.** Blocked: there are no privileged tool groups and no
  ACP.
- **Leak private data.** Blocked:
  - the liaison's include chain holds only operator-curated public
    information plus its own namespace;
  - there are no outbound tools other than the configured remote agents.
- **Persist the injection into privileged sessions.** Blocked: the
  liaison's namespaces may not appear in any privileged profile's chain.

The worst outcome that remains is accepted:
- the liaison sends the guest agent a wrong or odd answer;
- the liaison discloses information from its public namespaces, which is
  public by definition.

## Approaches considered

- **Shared or remote memory namespaces over JSON-RPC.** This was the first
  draft. It moves stranger-written records into the private instance's memory
  and needs a new API on the guest side. Dropped (see above).
- **MCP.** The guest would expose tools to the private model. Same exposure as
  A2A, plus a new server surface on the guest. Rejected.
- **A2A, private side as client.** Chosen. The guest side already serves
  `/a2a` (`SendMessage`, with `contextId` resumption and bearer-to-profile
  auth). The new code is a client tool and the validation rules, all on the
  private side.

## Decided design

### Private side (client)

1. New host config table
   `[remote_agent.<name>] = { endpoint, token_env, description? }`.
   - `endpoint` is the remote instance's `/a2a` URL.
   - `token_env` names the environment variable that holds the bearer token.
   - `description` tells the model who this agent is.
   - The whole table is **host-only**: the workspace config layer may not set
     it, which means extending `HOST_ONLY_PATHS`.
   - A missing env var for an agent some session profile uses is a startup
     error.
2. New `[session_profile.<p>].remote_agents = ["guest", …]`. Only these
   sessions get the tool, and only for the listed agents.
3. New ungrouped tool `a2a_send(agent, message, context_id?)`.
   - It sends a text `Message` via `SendMessage` and returns the remote agent's
     reply text and the `contextId` to continue with.
   - It is offered only in sessions whose profile lists remote agents, and
     `agent` must be one of them.
   - The tool description and result label the reply as untrusted text from
     another agent.
   - Its timeout is generous because the remote runs a whole turn (builder's
     choice, about 120 s).
   - There are no retries. Errors (unreachable, timeout, auth, remote error,
     non-terminal task state) are tool errors that name the agent.
4. **Privilege checks (startup; operator decision 2026-11-25).** For a
   session profile with any `remote_agents`:
   - **Error:** its permission profile lists `admin`. Admin tools edit the
     config itself, so an injected instruction could remove every other
     check.
   - **Warning** (one line each, naming profile and cause), not an error:
     - its permission profile lists `fs`, `shell` or `mcp`;
     - it is `[acp].stdio_session_profile`, or devices/keys are bound to it
       (the editor supplies fs and terminal tools; an outside caller could
       drive `a2a_send`);
     - outbound tools are offered in it (rule 5).
   - Rationale: fs or shell can already bypass any file-level rule, so
     forbidding one while allowing the other buys nothing; general A2A uses
     legitimately need them. The operator configures the liaison with none
     of them; the warnings tell them when they have not.
   - The same checks also appear in `verify`. No runtime refusals are added:
     a key bound later with `api-key add` is covered by `verify`'s warning.
   - The rule holds whichever config layer set the values.
5. **Outbound tools.** Ungrouped tools that send model-chosen data off the
   host — today `web_search`, `weather`, `skill_install`, `skill_update` —
   get a marker on the tool spec (e.g. `outbound: bool`, so a future tool
   opts in consciously). They stay offered; a `remote_agents` profile
   offering them gets a startup warning. A per-profile way to switch them
   off is in scope if cheap (e.g. `[session_profile.<p>].outbound_tools =
   false`); builder's call.
6. **Read-only import (operator decision 2026-11-25).**
   `[memory_namespace.<n>].import = ["public", …]` chains a namespace like
   `include` (prompt injection, tool reads, `# Memory` list) but the memory
   tools refuse to write it.
   - Read-only propagates: everything reachable *through* an import is
     read-only for this chain. A namespace is writable if at least one path
     to it uses only `include` edges (so the operator can both include
     `public` directly and still write it, while the liaison imports it).
   - The `# Memory` list marks read-only entries, e.g.
     `- public (read-only) — …`.
   - `import` and `include` share the DAG: cycles across either are refused,
     as today.
   - Background writers are unaffected (they write the home namespace).
   - It is a general feature, not liaison-specific.
7. **Containment checks (startup, warnings).** For a `remote_agents`
   profile:
   - warn if a namespace it can **write** (home or include-reachable)
     appears in any other profile's chain — content written from a remote
     conversation would be auto-injected there;
   - warn if its chain reaches another profile's home namespace.
   The intended liaison setup (`liaison` home, `import = ["public"]`)
   produces no warnings.
8. The liaison's own daily log and digests stay in its own namespace, which
   rule 7 checks. Nothing else changes for background tasks.

### Guest side (server) — configuration only

- Issue a token (`api-key add --profile owner_liaison`) bound to a session
  profile `owner_liaison`. That profile has its own namespace and the guest
  instance's ordinary memory, so it can report inquiries and record answers.
- `AGENTS.owner_liaison.md` (per-profile prompt file) states:
  - this caller is the owner's agent;
  - answers it gives may be used for guest replies;
  - what the guest agent should report when asked for the day's inquiries.
- Keep the guest `/a2a` reachable only from the private instance's network
  path (e.g. Tailscale), separate from the public form's path. The operator
  owns transport security, and plain `http://` is allowed.

### Verify / docs

- `verify` lists remote agents with their endpoint and whether the token env
  var is set, plus the warnings of rules 4 and 7. It does not call them.
- Driving the liaison: a channel room (Matrix/Discord/Telegram) bound to the
  liaison profile, and/or a timer run (operator decision 2026-11-25). No new
  client surface; `chat --profile` is a separate feature.
- `config.example.toml` gets a commented two-instance example: private
  `[remote_agent.guest]` + `[session_profile.liaison]`, and guest
  `owner_liaison`.
- The README gets a short paragraph, including the threat-model summary
  above.

## Left to the builder

- Whether `a2a-lf` 0.3 offers a client, or the request is hand-built with the
  crate's existing `reqwest`.
- The JSON shapes the tool returns and the exact error wording.
- How the per-turn filter learns the session profile; it is already scoped
  for config tools.
- Whether `contextId`s are remembered for the model (e.g. the last one per
  agent per session) or left to the model to carry.
