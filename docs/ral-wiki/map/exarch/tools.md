---
generated_at_commit: 6049f131
generated_at_date: 2026-09-22
covers_paths: [exarch/src/shell_eval/tools.rs, exarch/src/shell_eval/tools/]
---

# Map: exarch / tools

**`ral` is exarch's tool.** Every other harness affordance the model once
reached as a provider-advertised `Tool` — spawning, messaging, cancelling,
scheduling, replying, and reading a reply — is now a ral builtin reached by writing ral inside
`ral` itself, per the
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
migration; see [[map/exarch/builtins|builtins]] for the verbs and
[[map/core/engine-protocol|engine-protocol]] for the desk they speak
through.

`shell_eval/tools.rs` holds the two-line seam that makes this parametric. A
`Tool` is a static record — name, description, schema, dispatch `fn` — and
`Toolset` is a `Copy` slice of them: `Toolset::offered(thinking)` is `ral`
alone or `ral` plus `thinking`; `Toolset::default()` is empty (`--chat`). The
agent carries one `Toolset`, and both `provider.complete` (which puts
`Toolset::wire()` on the request) and `Avatar::invoke` (which dispatches through
`Toolset::get`) read it, so what was advertised and what is recognised cannot
disagree; an unadvertised name earns `unknown tool` and a `Forensic::Error`.
Every fork and desk spawn inherits its parent's set verbatim. Malformed input is
answered through the shared `required_str`/`input_error`, in the same words
for every tool.

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
- **`thinking`** (`shell_eval/tools/thinking.rs`) — offered only under the
  hidden `--thinking-tool` flag: a relay whose one field, `thought`, is recorded
  as a single `Display::Thinking` — the same lane the provider's own reasoning
  commits on, so a thought wears the `∴` rail, the drained ink, and the
  `/thinking` dial (newline-terminated, since the view fold grows one thinking
  block by every thinking record that follows) — and answered with the
  bare `relayed`. Nothing else happens — no call row, no model-view twin — so a
  model may narrate between calls without ending its turn. A record failure is
  a seam fault, not a tool error: the model still gets its acknowledgement.
- **`shell_eval/tools/agent.rs`** — no longer a tool module, but the
  fork-detach-register spine every launch shares: `spawn_async`, `AsyncSpawn`,
  `SpawnedChild`. Both `/branch`'s `spawn_branch` and the desk's
  `` exarch-agents `start `` handler build on it — either arm of it, in-process or
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
their value *is* the returned record, the whole read-only
`` `exarch-transcript `` family among them. There is no `Gate`/`tools_for` axis any
more — a fresh model never even sees a verb the desk would certainly refuse:
`agents` is dropped from the per-agent builtin index when the agent neither
spawns nor returns (its `` `reply `` needs only `returns`, so a fuelless
returning leaf keeps the verb), and the
self-wakeup family when the agent lacks the schedule grant (`prompt.rs`'s
`BuiltinIndex`, resolved once against the boot shell), while authority
itself is still enforced only at the desk, never by omission.
