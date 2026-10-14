# USER.md — User Model

Stable preferences and profile facts, written as directives that guide
future sessions. Durable facts that are *not* about the user go in
`memory/<namespace>/MEMORY.md` instead.

One directive per entry, each with a metadata line:

```md
<!-- observed: 2026-01-31 | status: active -->

- Prefer concise progress updates during implementation work.
```

- Begin each directive with an imperative: `Always`, `Never`, `Prefer`.
- Record the date you observed it, and either `active` or `superseded`.
- When a preference changes, mark the old entry `superseded` and rewrite
  the active one in place. Never leave two contradictory active directives.

## Directives

_(none yet)_
