---
generated_at_commit: ca2674822c667e858a566d84301a3c29c48012b7
generated_at_date: 2026-07-19
covers_paths: [exarch/src/shell_eval/tools.rs, exarch/src/shell_eval/tools/]
---

# Map: exarch / tools

**`ral` is exarch's one tool.** Every other harness affordance the model once
reached as a provider-advertised `Tool` — spawning, messaging, cancelling,
scheduling, and `reply` — is now a ral builtin reached by writing ral inside
`ral` itself, per the
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
migration; see [[map/exarch/builtins|builtins]] for the verbs and
[[decisions/260706_enquiry-channel|enquiry-channel]] for the desk they speak
through. `shell_eval/tools.rs` shrinks to:

- **`ral`** (`shell_eval/tools/ral.rs`) — the one call that crosses the provider
  boundary: evaluate ral source against the session shell, synchronously,
  through [[map/exarch/shell-eval|`run_shell`]]. Its input is a required `cmd`
  (the ral source) and a required one-line `description` (shown on the
  [[map/exarch/frontend|rail]]; the full `cmd` opens in the collapsible
  tool-call block; oversize descriptions are truncated, never rejected). An
  optional `timeout_secs` bounds the call, defaulting to `CALL_TIMEOUT_SECS`
  (60s) — a default, not a cap: raise it for known-long work, or `spawn` what
  should outlive the turn.
- **`shell_eval/tools/agent.rs`** — no longer a tool module, but the
  fork-detach-register spine every launch shares: `spawn_async`, `AsyncSpawn`,
  `SpawnedChild`. Both `/branch`'s `spawn_branch` and the desk's `agent-start`
  handler build on it, so `/branch` and the harness spawn verb share one
  mechanism ([[design/agents|agents]], [[map/exarch/agent|agent]]).

The harness verbs are answered by the `ExarchDesk` (`exarch/src/fleet/desk.rs`),
installed per `ral` call and reached through `shell.enquire(...)` from the
builtin's body; acting verbs keep `Kind::HarnessCall`/`HarnessResult` rail
chrome (spawns additionally derive a child tab), listings stay silent since
their value *is* the returned record. There is no `Gate`/`tools_for` axis any
more — a fresh model never even sees a verb the desk would certainly refuse:
`reply` is dropped from the per-agent builtin index when `!returns`, and the
self-wakeup family when the agent lacks the schedule grant
(`prompt::resolve_builtin_index`), while authority itself is still enforced
only at the desk, never by omission.
