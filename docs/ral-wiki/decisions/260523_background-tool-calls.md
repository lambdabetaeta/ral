---
status: active
---

# Background tool calls for exarch

> The in-turn two-pass staging phase is active through [[decisions/260616_tool-boundary-steering|batch-boundary steering]]; true turn-outliving background work remains split across later ADRs.

A study of running long-running tool work (a shell command, a subagent) in the
background while the parent turn proceeds, in the spirit of Claude Code's
`run_in_background`. It maps the constraints — Provider lifetime, the bus
channel, scope vs `spawn`, lifecycle and cancellation — and lays out a phased
plan: two-pass staging, `Arc` the Provider, a backgroundable shell, auto-notify,
a backgroundable agent, cleanup. Full study:
`dev/docs/260523_background_tool_calls.md`.


## Landed

The two-pass staging phase is in: `Session::dispatch` opens a single
`thread::scope`, stages every call into a `Vec<Staged>`, then joins in a second
pass. A batch of `[agent, agent, agent]` therefore runs its children
concurrently, joined before the parent turn can continue.

[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] now drains the
prompt queue only after that whole tool-call batch, so queued steering redirects
the next assistant step without serialising sibling agents.

## Open

The lifetime work that backgrounded children would require has not been done:

- The Provider is still borrowed `&Provider` through `Session::stage` and the
  agent tool's spawn closure — there is no `Arc<Provider>`, so no worker can
  outlive the spawning turn.
- `Staged` has exactly two variants (`Done`, `Spawned`); there is no "register
  and return" shape, no job registry, no `JobId`.
- The `shell` tool dispatches synchronously (`run_shell` under `Staged::Done`);
  its only knob is `timeout`, with no `background` flag or
  `shell_spawn`/`shell_collect`/`shell_kill`.
- There is no auto-notify drain and no per-job cancel — cancellation remains the
  single process-global flag (`cancel::is_set`).

So the spawned children of a turn are still bound to that turn's `thread::scope`:
the parent turn cannot complete ahead of them, and nothing survives into a later
turn. Restored same-batch concurrency does not by itself make them background
work. Widening past this — Provider sharing, a job registry, a backgroundable
shell, auto-notify — stays a direction to take only when the long-running-shell
workflow proves to be the bottleneck it appears to be from the outside.
