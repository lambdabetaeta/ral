---
generated_at_commit: e0e912dc
generated_at_date: 2026-06-17
covers_paths: [exarch/src/tools.rs, exarch/src/tools/]
---

# Map: exarch / tools

`tools.rs` is the tool registry. A `Tool` advertises itself to the
[[map/exarch/provider|provider]] (name, description, JSON schema) and knows how
to `dispatch` one parsed JSON input against a live [[map/exarch/session|`Session`]].
Each tool owns its own input parsing and invalid-input UX; nothing in
`provider.rs` or `session.rs` knows a tool's shape. Synchronous tools return
`Staged::Done`; forking tools return `Staged::Spawned`, and the session joins
them after staging the whole assistant tool-call batch. Adding a tool is a
sibling module under `tools/` listed in `registry()`.

Three tools ship:

- `shell` (`tools/shell.rs`) — evaluate ral source against the session shell;
  runs synchronously through [[map/exarch/shell-eval|`run_shell`]]. Its input
  carries a required `cmd`, a per-call `timeout` (1–3600s, no default — sizing it
  is part of issuing the command), a required one-line `description` (shown on the
  [[map/exarch/frontend|rail]]; the full `cmd` is revealed when the user opens the
  collapsible tool-call block), and an optional `audit`.
- `agent` (`tools/agent.rs`) — `fork` a child [[map/exarch/session|session]] from
  a value-snapshot of the parent shell and run a sub-prompt to completion,
  returning its final text. Returns `Staged::Spawned`; the parent dispatch loop
  stages every same-batch call before joining, so sibling agents can overlap
  while still finishing before the parent turn continues. Emits
  `Born` / `Died` / `SubagentDone` on the bus so the TUI can tab and breadcrumb
  children.
- `fff` (`tools/fff.rs`) — frecency-ranked fuzzy filename search over the working
  tree via the `fff-search` crate. The first call per directory blocks on a
  background scan, then caches a `FilePicker` (scan thread + filesystem watcher)
  in a process-global registry keyed by canonical path; later calls — including
  forked children sharing the cwd — reuse the live index. Databases live under a
  per-pid `$TMPDIR` directory.

Editing and content search are *not* tools: the model runs them as ral host
atoms inside `shell` — see [[map/exarch/builtins|builtins]].
