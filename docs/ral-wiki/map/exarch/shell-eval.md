---
generated_at_commit: 2a3d8a5
generated_at_date: 2026-07-27
covers_paths: [exarch/src/shell_eval.rs, exarch/src/shell_eval/builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / shell eval

`shell_eval.rs` runs one tool call as a ral top-level run against the
persistent [[map/core/shell-state|`Shell`]]. **`run_shell` is a pure *request
supplier*: it builds a transport-level `Source` `Run`, dispatches it through
`ral_core::transport::dispatch_to_report` against the agent's seat transport —
the in-process `IdentityTransport`, or a wire engine's `WireTransport`
([[map/exarch/agent|agent]]) — and renders the terminal `Report` that comes
back** — the
transport is the canonical run vocabulary
([[decisions/260706_enquiry-channel|enquiry-channel]];
[[internals/a-turn-end-to-end|a run, end to end]];
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]). Evaluation can
be entered *only* through a framed run door — the reduction primitive behind it
is crate-private ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]) —
so core owns all the run machinery (compile, frame install, capture, the wall),
and `run_shell` owns only the run it builds and the outcome it formats:

- **source + `script_name: "<tool>"`.** Core's `compile_run` runs
  `compile_and_typecheck` seeded from the live session (`shell.session_schemes()`,
  the one name→scheme seed —
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]); the
  prelude's schemes ride scope[0], installed when the annotated prelude was
  evaluated at boot. The check is strict — any type error is fatal — over the
  single mode-inference engine every evaluated path shares
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).
  Parse/type errors come back as `Report::Static { diagnostics }`, which
  `run_shell` formats to `Outcome::Static`; on success the *annotated* comp
  runs ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]);
- **`caps`** — the session `Capabilities`, pushed for the eval's dynamic extent.
  **This is the sandbox**: the boundary is the pushed [[design/grant|grant]]
  frame plus the [[map/core/evaluator|top-level contract]], not a source-level
  `grant { … }` the model could escape. External commands route through the same
  OS sandbox as ral ([[map/core/capabilities|capabilities]]). The post-run
  `Mobile` installs onto the shell, so `let`, `cd`, and env persist across tool
  calls (the in-module tests pin this);
- **`io: RunIo::Capture`** — core mints the stdout/stderr buffers and returns
  them in `Report::Ran`'s `captured`: the full, model-visible and logged
  text. Nothing echoes live; the [[map/exarch/frontend|rail]] surfaces cards
  instead, and the [[map/exarch/agent|digest]] caps shape only the model's
  history view;
- **`terminal: RequestedTerminalAccess::Denied`** — a tool run holds no
  [[decisions/260619_terminal-lease|terminal lease]], so the foreground handoff
  is *uninvocable*: a bare pipeline cannot `tcsetpgrp`, and the SIGTTIN crash of
  an agent stealing the controlling terminal becomes a state the types refuse to
  represent. Paired with `stdin: RunStdin::Empty`, the run reads no terminal at
  all — an explicit empty source, not a side effect of foreground denial;
- **`wall`** — the per-tool deadline, the `ral` tool's `timeout_secs` (60s
  default). Core arms it on the run's foreground scope *before compiling*, so
  it bounds the whole run; only `CancelCause::Deadline` reports `timed_out`,
  which `run_shell` turns into the timeout-124 message (retry with a higher
  `timeout_secs`, or `spawn` and overlap), while Esc stays an interrupt. A grant body evaluates locally — no sandbox-IPC
  round trip to interrupt — so cancellation reaches any spawned child through the
  ordinary process-group / cancel-scope path;
- **`deferred_lease`** — the idle-observation lease for workers the run
  detaches at the durable root: `DETACHED_WORKER_CEILING` (1 h) as the idle
  bound — a worker is reaped once unobserved that long, renewed by any
  `poll`/`await`/`race` naming its handle — under `DETACHED_WORKER_BACKSTOP`
  (24 h), the absolute age no polling extends. Not load-bearing for run
  exit;
- **`worker_cap`** — `LIVE_WORKER_CAP` (64), the admission bound core
  enforces at the spawn door: the 65th spawn is refused (the error names
  `await`/`cancel`) while 64 workers of any class still run; settled
  entries lingering under retention hold no seat. Its sibling
  constant `SETTLED_WORKER_RETENTION` (256 ral calls, matching the binding
  lease's scratch expiry) is not on the request at all — it is armed once
  through `Shell::arm_worker_retention`, and the sweep it parameterises is
  engine housekeeping;
Surface delivery is not a `Run` field: `dispatch_to_report` takes the live and
deferred-batch sink closures directly (below, both routed through
`fleet::desk::SurfaceApplier`), plus an enquiry handler answering through the
per-call `ExarchDesk` when one is installed ([[map/exarch/builtins|builtins]]);
a desk-less dispatch gets an honest `EnquiryError`.

`BINDING_IDLE_CALLS` (256, beside `DETACHED_WORKER_CEILING`) is the other
lease constant this module owns but does not put on the request: it is not
per-run policy, it is per-*shell* policy, armed once by
`bootstrap::arm_session_ledgers` — the one ledger-policy site, applied by
the identity seat's ceremony and the wire engine's boot recipe alike
([[map/exarch/agent|agent]]; [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]).
Reusing the settled-worker-retention figure is deliberate: one ral-call
clock, read by both ledgers for their own idle policy. `LARGE_BINDING_BYTES`
(1 MiB) rides the same `BindingLease` at the same arming site — a
residency threshold, not a lifetime one, so the install chokepoint checks it
independently of idle age or baseline status.

Completion is `dispatch_to_report` returning its `Report`. A detached `spawn`ed worker — a
server, a watch — holds bounded deferred surface storage in core, never a clone
of the bus [[map/exarch/frontend|`Emitter`]], so it cannot keep the tool run
from ending ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). Frame
teardown — restoring the prior `RunState`, its byte sinks, its surface, and its
terminal access — is core's `RunGuard`, which self-heals on a caught worker
panic as well as on the normal return
([[decisions/260612_exarch-panic-recovery|panic-recovery]]); exarch installs no
per-call `IoGuard` of its own. The dynamic-context half of the contract lives in
[[map/exarch/agent|agent]].

**Surface decoding.** `decode_surface` is the single decoder both delivery
regimes share: the live foreground sink `run_shell` hands `dispatch_to_report`
emits now, and the deferred sink (`deferred_sink`, installed on the transport
before each dispatch) mints identical events when a detached worker's batch
flushes. `accepted_surface` wraps it with the protected-pin guard; each decoded
`Kind` reaches the [[map/exarch/frontend|bus]] through the call's `Emitter`.
Five shapes ride the one `surface` channel, tried pin-first:

- a `` `pin ``/`` `unpin `` wrapper decodes to `Kind::Pin` / `Kind::Unpin`, but
  the host-owned `services` key is protected
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]):
  ordinary `surface` writes or clears there are rejected with a diagnostic
  before they reach the pin mirror or viewport; accepted pins are mirrored as
  `PinDigest`s so the [[map/exarch/agent|nudge]] layer can name pinned state
  without parsing rendered text;
- an `io`-keyed `Map` core emits at a redirect / exec door decodes through
  `value_to_io` / `io_card` into a `Kind::Io { event, card }`, carrying the raw
  effect record beside its rendering ([[map/exarch/io-surface|io-surface]]);
- a `` `notice `` core's ready-boundary housekeeping pushes (a worker reap, an
  idle-binding prune, a large-binding warning) decodes to
  `Kind::Notice { notice, card }` ([[decisions/260706_enquiry-channel|enquiry-channel]]);
- any other value tries `value_to_card` and becomes a `Kind::Card`; the closed
  mark set and the `value_to_card` / `render_card` decode-and-bind path live in
  [[map/exarch/cards|cards]];
- the `` `done `` completion event a detached worker flushes at the end of its
  batch decodes to `Kind::Done { outcome, card }`;
- a value that is none of these is dropped, the same graceful degradation
  `value_to_card` gives an unknown mark.

The producer is a direct `surface` call at each kit site, with no cross-language
sentinel constant. Same-thread children inherit the sink; detached workers do
not — core buffers their `surface` calls and replays them once on `await`, so a
bus `Emitter` clone can never outlive the tool run. Across the OS-sandbox
boundary the events are buffered in the confined child and replayed through the
parent's sink ([[map/core/capabilities|carried on the IPC response]]), so they
are batched rather than live under the sandbox.

`shell_eval/builtins.rs` registers exarch's resident host atoms — `view-text`, the
`grep-files` search, the hash-addressed `edit-hash`/`edit-replace`, whose
file I/O happens in Rust, below the redirect frame
([[map/exarch/io-surface|io-surface]]) — and sources the small embedded
`data/agent.ral` helper library (`view-text-around`, the tasks kit) into the
shell at boot ([[map/exarch/builtins|builtins]]). The one `host_surface()`
value declaring these sets rides core's `boot_shell` at construction and is
also the builtin surface a wire engine child's `Frame::Attach` names, so a
remote shell is dressed with the same atoms.

A tool command that fails under an active OS sandbox carries a kernel-denial
diagnostic — the blocked syscall, the exact path to grant, the symlink caveat —
appended to the error's `hint`. That harvesting now lives in core
(`core::sandbox::diag`), driven by the command and pipeline runners over the
failing call's wall window and rendered identically by both the `ral-sh` REPL and
exarch ([[map/core/capabilities|capabilities]]).
