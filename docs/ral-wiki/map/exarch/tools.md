---
generated_at_commit: 99300c0
generated_at_date: 2026-06-21
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
  a value-snapshot of the parent shell and run a sub-prompt. Bimodal
  ([[decisions/260617_async-agent-tool|async-agent-tool]]): `sync` (the default)
  is a *dependency edge* — `Staged::Spawned`, joined before the parent's next
  provider request, its final text returned in the `tool_result`, so same-batch
  siblings overlap but never outlive the turn; `async` is an *orchestration
  edge* — a detached registry-owned worker (`agents` lists live ones,
  `agent_cancel` stops one) that returns a start receipt now and delivers its
  reply later as a marked `Turn` through the [[map/exarch/frontend|inbox]]. A
  child's settle reduces once through `AgentOutcome` (one `run_child`), drawn as
  the `↘` `SubagentDone` breadcrumb in both modes. Both stream
  `Born` / tokens / `Died` to a live tab — sync always, async whenever the bus
  is session-lived ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
- `fff` (`tools/fff.rs`) — frecency-ranked fuzzy filename search over the working
  tree via the `fff-search` crate. The first call per directory blocks on a
  background scan, then caches a `FilePicker` (scan thread + filesystem watcher)
  in a process-global registry keyed by canonical path; later calls — including
  forked children sharing the cwd — reuse the live index. Databases live under a
  per-pid `$TMPDIR` directory.

Editing and content search are *not* tools: the model runs them as ral host
atoms inside `shell` — see [[map/exarch/builtins|builtins]].
