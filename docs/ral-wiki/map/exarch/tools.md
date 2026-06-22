---
generated_at_commit: f5dccde
generated_at_date: 2026-06-22
covers_paths: [exarch/src/tools.rs, exarch/src/tools/]
---

# Map: exarch / tools

`tools.rs` is the tool registry. **A `Tool` advertises itself to the
[[map/exarch/provider|provider]] (name, description, JSON schema) and dispatches
one parsed JSON input against a live [[map/exarch/session|`Session`]], returning
a `SessionToolResult` synchronously** — every tool returns now, so there is no
join phase. Each tool owns its own input parsing and invalid-input UX; nothing
in `provider.rs` or `session.rs` knows a tool's shape. Adding a tool is a
sibling module under `tools/` listed in `registry()`.

**Two mirror-image axes decide which tools a session holds**, gating both
advertisement (`provider.complete`) and dispatch (`Session::stage`) through one
`ToolSet::allows` check:

- `spawns()` is true for the spawn family; a peer's `ToolSet::NoSpawn` withholds
  it, so the spawn tree stays one level deep.
- `replies()` is true for `reply`; the root's `ToolSet::All` withholds it — the
  root talks to the user across turns and never returns a value.
- So the *root* holds everything but `reply`, and a *peer* holds everything but
  the spawn family — keeping `reply`, its way of returning.

The tools that ship:

- `ral` (`tools/ral.rs`) — evaluate ral source against the session shell,
  synchronously, through [[map/exarch/shell-eval|`run_shell`]]. Its input is a
  required `cmd` (the ral source) and a required one-line `description` (shown on
  the [[map/exarch/frontend|rail]]; the full `cmd` opens in the collapsible
  tool-call block). A fixed 30s call timeout bounds inline work; anything longer
  belongs in a `spawn` that outlives the turn.
- the **spawn family** — `agent` / `agents` / `agent_cancel` (`tools/agent.rs`),
  gated by `spawns()`. `agent` is launch-only and always asynchronous
  ([[decisions/260617_async-agent-tool|async-agent-tool]]): it `fork`s a child
  [[map/exarch/session|session]] from a value-snapshot of the parent shell, runs
  it on a detached thread through the same `Session::drive` loop, and returns a
  start receipt at once; the child's single reply is delivered later as a marked
  `Turn` through the [[map/exarch/frontend|inbox]]. `agents` lists live workers
  (id, title, elapsed, log dir); `agent_cancel` stops one by id. A peer is denied
  the family, so the tree stays one level deep.
- `reply` (`tools/reply.rs`), gated by `replies()` — a sub-agent's deliberate
  return value ([[decisions/260622_agent-reply-tool|agent-reply-tool]]). Its
  `result` argument is rendered by the shared value→text rule
  ([[map/exarch/shell-eval|shell-eval]]'s `json_to_text`: a string passes through
  raw, an object/array is pretty-printed), stashed on the session, and lifted
  into a `Replied` terminal once the tool-call batch drains — it hard-terminates
  the child. There is no prose scrape, so a child that never calls `reply`
  returns nothing. Withheld from the root.
- the **schedule family** — `schedule` / `schedules` / `unschedule`
  (`tools/schedule.rs`) — self-armed wakeups (a cron expression or `after <dur>`)
  posted into the session's *own* inbox. Gated not by `ToolSet` but by
  `schedule_authority`, so a peer may wake itself when its root was launched
  `--allow-schedule`.
- `fff` (`tools/fff.rs`) — frecency-ranked fuzzy filename search over the working
  tree via the `fff-search` crate. The first call per directory blocks on a
  background scan, then caches a `FilePicker` (scan thread + filesystem watcher)
  in a process-global registry keyed by canonical path; later calls — including
  forked children sharing the cwd — reuse the live index. Databases live under a
  per-pid `$TMPDIR` directory.

Editing and content search are *not* tools: the model runs them as ral host
atoms inside `ral` — see [[map/exarch/builtins|builtins]].
