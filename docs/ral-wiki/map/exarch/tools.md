---
generated_at_commit: fb52275a
generated_at_date: 2026-08-25
covers_paths: [exarch/src/shell_eval/tools.rs, exarch/src/shell_eval/tools/]
---

# Map: exarch / tools

**`ral` is exarch's one tool.** Every other harness affordance the model once
reached as a provider-advertised `Tool` — spawning, messaging, cancelling,
scheduling, replying, and reading a reply — is now a ral builtin reached by writing ral inside
`ral` itself, per the
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
migration; see [[map/exarch/builtins|builtins]] for the verbs and
[[map/core/engine-protocol|engine-protocol]] for the desk they speak
through. `shell_eval/tools.rs` shrinks to:

- **`ral`** (`shell_eval/tools/ral.rs`) — the one call that crosses the provider
  boundary: evaluate ral source against the session shell, synchronously,
  through [[map/exarch/shell-eval|`run_shell`]]. Its input is a required `cmd`
  (the ral source) and a required one-line `description` (shown on the
  [[map/exarch/frontend|rail]]; the full `cmd` opens in the collapsible
  tool-call block; oversize descriptions are truncated, never rejected). An
  optional `timeout_secs` bounds the call, defaulting to `CALL_TIMEOUT_SECS`
  (60s) — a default, not a cap: raise it for known-long work, or `spawn` what
  should outlive the run. A call the wall cuts short loses its bindings and
  keeps its acts, so its stderr carries the engine's span, the remedy, an audit
  of what already stands, and the `defer`red workers still running past the wall
  with nothing left to `await` them by
  ([[map/exarch/shell-eval|shell-eval]]).
  Every accepted or malformed call emits a `Display::ToolCall` followed by a
  `Display::Result` addressed by the call's `BlockId`, so a result never has to
  search backward for its tool row. Malformed JSON uses `<invalid input>` as the
  call label but still gets a paired diagnostic result; if the call row itself
  cannot be appended, the seam reports a transient fault and cannot invent a
  result target.
- **`shell_eval/tools/agent.rs`** — no longer a tool module, but the
  fork-detach-register spine every launch shares: `spawn_async`, `AsyncSpawn`,
  `SpawnedChild`. Both `/branch`'s `spawn_branch` and the desk's
  `` agents `start `` handler build on it — either arm of it, in-process or
  across a wire — so `/branch` and the harness spawn verb share one
  mechanism ([[design/agents|agents]], [[map/exarch/agent|agent]]).

The harness verbs are answered by the `ExarchDesk` (`exarch/src/fleet/desk.rs`),
installed per `ral` call and reached through `shell.enquire(...)` from the
builtin's body; acting verbs emit `Display::HarnessCall`/`Forensic::HarnessResult` and, on
the arm where the act genuinely landed, file it in the call's act ledger for the
audit every raise owes the model ([[map/exarch/shell-eval|shell-eval]]).
They are rendered as **acts** — verb, subject, payload rows that never fold into
an observation run
([[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]; spawns
additionally derive a child tab) — while listings stay silent since
their value *is* the returned record. There is no `Gate`/`tools_for` axis any
more — a fresh model never even sees a verb the desk would certainly refuse:
`agents` is dropped from the per-agent builtin index when the agent neither
spawns nor returns (its `` `reply `` needs only `returns`, so a fuelless
returning leaf keeps the verb), and the
self-wakeup family when the agent lacks the schedule grant (`prompt.rs`'s
`BuiltinIndex`, resolved once against the boot shell), while authority
itself is still enforced only at the desk, never by omission.
