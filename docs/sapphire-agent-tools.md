# sapphire-agent tools

The agent's own tools, and the outbound MCP servers in `[[tools.mcp_servers]]`
(`mcp__<name>__<tool>`), are configured on the host; see
`crates/sapphire-agent-server/config.example.toml` and the README's "Zed / ACP"
section. This page covers attachment and image tools, plus tools supplied by
an ACP client.

## Attachments and local images

| Tool | Purpose |
|---|---|
| `attachment_save(attachment_id, path)` | Save the original bytes of an attachment received in the same Discord / Matrix conversation (including its thread). |
| `image_read(path)` | Read one JPEG / PNG / GIF / WebP on the agent's machine and return it as an image to the model. |

Both require `tools.host_access.enabled = true` and are unavailable in ACP
sessions: they never fall back to the agent's filesystem from ACP. `file_read`
remains text-only. `image_read` accepts workspace-relative, absolute and `~/…`
paths, with a 32 MiB input limit; images over 5 MiB are shrunk for display
without modifying the file. Unsupported formats are reported as text.

Discord / Matrix attachments, including non-image files, are automatically
fetched on receipt. Limits are **32 MiB per file, 8 attachments per post,
64 MiB per post**, and **1 GiB for the temporary original store**. Accepted
originals are retained for **7 days**: use the attachment ID in the message's
note with `attachment_save` before expiry, and supply an explicit destination.
Saving overwrites an existing destination; saved files are not automatically
deleted when the temporary original expires.

Capacity shortages, download failures and missing cached originals are
reported as notes. The original saved by `attachment_save` is separate from
any resized image shown to the model. These limits do not guarantee a
streaming memory bound for Matrix: its SDK buffers the whole download before
the returned size can be checked.

## Tools a client provides (`acp__…`)

An ACP client can bring its own tools into a session: a game offering the
agent `poke` or `look`, an editor offering something of its own. It does this
by running an MCP server itself and handing it to the agent in `session/new`
(or `session/load` / `session/resume`). Nothing in the agent's config names
these servers.

### Only MCP over ACP

The agent accepts a client's MCP server only over ACP itself (MCP over ACP):
the `mcpServers` entry is `{ "type": "acp", "name": …, "serverId": … }`, and
the agent talks MCP to it in `mcp/message` requests on the same ACP
connection. `initialize` advertises this as
`agentCapabilities.mcpCapabilities.acp: true`.

`stdio`, `http` and `sse` entries are ignored with a warning: the agent does
not start processes or open connections on a client's say-so. An MCP server
the agent itself should use goes in `[[tools.mcp_servers]]`.

With the Rust ACP SDK (`agent-client-protocol` 5.1 with
`unstable_mcp_over_acp`, plus `agent-client-protocol-rmcp`), a client attaches
an MCP server to a session with `with_mcp_server`, and the SDK does the rest:
it declares the `acp` entry and answers `mcp/message`. The test
`client_provided_mcp_tool_round_trip` in
`crates/sapphire-agent-server/src/serve/acp.rs` is a working example.

### Names

Each tool is offered to the model as `acp__<server>__<tool>`, where `<server>`
is the entry's `name` and `<tool>` the MCP tool's name.

- Both parts are made safe for a tool name: every character other than ASCII
  letters, digits, `_` and `-` becomes `_` (`my game` → `my_game`). Names are
  not shortened, the same as `mcp__…` tools.
- In `<server>`, a run of `_` is then cut to one and a `_` at either end is
  dropped (`a__b` → `a_b`, `game_` → `game`), so a server name never holds
  `__`, the separator: no two servers' tools can end up with one name, and
  refreshing one server's tools never touches another's.
- When two servers in one session end up with the same name, the later ones
  get `_2`, `_3`, … (`game`, `game_2`, `game_3`).
- When two tools of one server end up with the same name (`a.b` and `a_b`
  both become `a_b`), the first keeps it and the later ones get `_2`, `_3`, …
  (`acp__srv__a_b`, `acp__srv__a_b_2`), with a warning in the log. The call
  still reaches the tool under its own name (`a.b`, `a_b`).

### Permission comes from the tool's annotations

A client's tool is classified by its MCP annotations, and that decides whether
the agent asks before running it, as for any other tool in an ACP session:

| Annotations | Classified as | `default` | `accept_edits` | `bypass` |
|---|---|---|---|---|
| `readOnlyHint: true` | read | run | run | run |
| `readOnlyHint` not true, `destructiveHint: false` | edit | **ask** | run | run |
| neither, or `destructiveHint: true` | unclassified | **ask** | **ask** | run |

A tool with no annotations is unclassified, the strictest bucket. A game whose
tools only look at or harmlessly change its own state can annotate them and
have them run without a prompt.

An "always allow" granted for an `acp__…` tool is kept like any other: per
room profile, by tool name, in the agent's ACP permissions store. So it also
covers a later session whose client offers a tool of the same name.

This trusts annotations that the outbound MCP servers do not get trusted
with (see "Trusting an outbound MCP server" in the README). Here the
annotations are the client's own word about its own tools, and all they
decide is whether the agent asks *that same client* before calling them. A
client that mislabels its tools only skips its own prompts.

### Where they are visible

- Only in the session whose request named the server, and only in that
  session's own turns. Other sessions on the same connection, other
  connections, and the agent's shared tool list never see them.
- Not in subagents: a subagent a turn starts gets the agent's tools, not the
  client's.
- For as long as the session is attached. The tools are listed once the
  session exists (the first turn waits up to 10 seconds for the list, then
  starts without it), and are dropped when the connection closes. They are
  not saved: `session/load` builds them again from the `mcpServers` that
  request brings.
