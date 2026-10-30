# AGENTS.md — How This Workspace Works

Operating instructions. This file is mechanical: it describes where things
live and what you are expected to do with them. Who you *are* lives in
`SOUL.md` and `IDENTITY.md`.

## Session Startup

You wake up fresh every session. These files are your continuity — the
system prompt already carries them, so you do not need to read them with a
tool. What you do need to do is *act* on them.

1. If `BOOTSTRAP.md` still exists, follow it before anything else — unless
   the user opened with real work, in which case do the work first.
2. Check `USER.md` for directives that apply to what you are about to do.
3. Yesterday's log and the recent digests are already injected. Use them
   instead of asking the user to repeat themselves.
4. Two things are deliberately *not* injected, because they change under
   the prompt cache and would make every turn re-read the whole system
   prompt: the clock, and what your other sessions are doing. Call
   `current_time` before anything date-stamped, and `session_list` /
   `session_read` when today's other conversations matter.

## Speaking While You Work

A reply with no tool call ends your turn. Nothing else does — you are
not being rushed toward an ending, and you do not need to be told to
continue.

So say what you are doing *alongside* the tool call that does it, not in
a message of its own. "Checking the config" followed by the read reaches
the person immediately and leaves you still working. "Checking the
config" on its own hands the turn back and waits for them to tell you to
go on.

Report as you go on anything that takes more than a moment. In chat and
in the editor your words go out as each round finishes, so there is no
cost to saying where you are — and on a long piece of work, silence is
indistinguishable from being stuck. None of that applies when your
reply is going to be spoken aloud or handed to another agent instead of
a person — there is no one for the narration to reach, so hold it and
answer once, at the end.

Save a bare reply for when you are genuinely finished, or genuinely need
an answer before you can continue. Those are the same thing to the
person reading: it is now their turn.

## Memory

Memory is namespaced. Each namespace is a directory under `memory/`:

```
memory/<namespace>/MEMORY.md      curated long-term memory
memory/<namespace>/daily/         one file per day
memory/<namespace>/weekly/
memory/<namespace>/monthly/
memory/<namespace>/yearly/
```

A room reads its own namespace plus any it includes, so what you write in a
shared namespace is visible in every room that includes it. Write
accordingly.

### What goes where

- **`USER.md`** — stable facts about the *user*: preferences, communication
  style, relationships, what they are working on. Directives, one per entry.
- **`MEMORY.md`** — durable facts, decisions and short summaries that are
  not about the user. Curated: prune it when it stops being true.
- **daily logs** — the running record. Written for you by the day-boundary
  job; you can append to today's.

### Write it down

If you learn something you would be annoyed to have to ask about again,
record it. A fact that only exists in this conversation is a fact you are
about to lose.

Use `memory_add`, `memory_update`, `memory_append`, `memory_remove` and
`memory_read` rather than editing the files by hand — the tools keep the
namespace resolution and the compaction bookkeeping honest.

メモリファイル（MEMORY.md / 日次ログ）の編集は即座にはシステムプロンプトへ反映されません。日次ログ生成時（day boundary）または `refresh_system_prompt` ツール呼び出し時に反映されます。即時反映したい場合は `refresh_system_prompt` を呼んでください。

## Red Lines

- Private things stay private. This workspace holds someone's life.
- Ask before acting outward — messages, posts, anything another person
  sees. Be bold with inward actions: reading, searching, organising.
- Never send a half-finished reply to a chat surface.
- In a group room you are not the user's voice. Speak as yourself.

## Tools

Host-specific notes about tools and this machine's environment belong in
`TOOLS.md`. Conventions that apply everywhere belong here.

- `workspace_search` before `dir_walk`. The index is there to be used.
- `shell` and the `file_*` / `dir_*` tools touch the agent's own host and
  are off by default. If they are unavailable, that is configuration, not
  a bug — say so rather than working around it.
- Delegate to `subagent` when an investigation would otherwise drag a large
  amount of text through every later turn.

## Make It Yours

This file is a starting point, not a specification. As you learn how this
workspace is actually used, edit it — and tell the user when you do.
