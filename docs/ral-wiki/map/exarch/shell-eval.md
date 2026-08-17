---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
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
  which `run_shell` turns into the exit-124 stderr described under *the wall is
  a place* below, while Esc stays an interrupt. A grant body evaluates locally — no sandbox-IPC
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
teardown is core's: `IoLoan` brackets the byte streams and registers a run takes
on loan from the shell, self-healing on a caught worker panic as well as on the
normal return ([[decisions/260612_exarch-panic-recovery|panic-recovery]]), while
the run's invariant half — surface, deferred sink, desk, nursery, cancel, the
leases — threads as an immutable `&Mooring` the stack itself restores. Exarch
brackets only its own per-call desk install (`seat::RunGuard`). The
dynamic-context half of the contract lives in [[map/exarch/agent|agent]].

**The wall is a place, and the acts before it stand.** A timed-out call unwinds
its bindings and keeps its effects: the child is running, the bytes are in the
inbox, the wakeup is armed, the staged `reply` is still harvested. Nothing
rolls back, and a model told only "retry with a higher `timeout_secs`" would
duplicate every one of them. So the `timed_out` branch writes four things to
stderr, in order, and exits 124:

1. **the engine's rendering, verbatim** — unconditionally, exactly as the
   ordinary-failure branch beside it. A cancel is stamped on the innermost node
   it unwound through ([[internals/cancellation|cancellation]]), so the
   diagnostic carries the span that *locates* the wall: the frontier between the
   steps that completed and the one that did not;
2. **the asymmetry and the remedy** — it timed out after *n* seconds at the
   point above; the steps before it completed, the step it names did not, and
   the bindings are gone. Then `recovery:` — raise `timeout_secs` for work that
   is simply slow, or `let h = defer { … }` and let the run return, since the
   host notifies at the next exchange boundary and `await $h` yields the value
   record without polling;
3. **an audit of what already stands**, when there is any — and not the wall's
   alone: *every* ending that discards the bindings files it, a chosen `exit` as
   much as a suffered raise, last, after whichever remedy the ending offered,
   because the asymmetry belongs to the unwind and not to the deadline (a call
   that staged its `reply` and then died on a command's non-zero exit, or chose
   `exit 2`, made that reply stand just as surely). `Break::Stopped` is job
   control rather than an ending, keeps its bindings, and files nothing. This
   is the one exception to core's own trail: the desk authors the shared
   `Observation` vocabulary from the *host* side, into a per-call fragment
   joined at render — never into the engine's trail, because a wire seat's
   cancel can land while the engine sits parked in `enquire`, mid-unwind of a
   builtin whose act the desk already committed, and engine-side recording
   alone would then report a standing act as failed. `HostServices::commit_act`
   is the one door every acting handler funnels through — minted in
   `Agent::host_services`, the one place a call's whole desk capture is
   assembled, so the fragment's extent *is* the call's — and it builds one
   `Observed::Act` per attempt, at the arm where the outcome is known, and fans
   it out itself: the rail's `Display::HarnessCall` row *always*, off the very
   `verb`/`subject`/`payload`/`refused` the observation carries, and the
   fragment *only* when `refused` is `false`. A refused attempt leaves no
   fragment entry: it answers one question — what stands — and an entry for
   work that never happened would blunt it. Because one datum feeds both
   readers, a seventh act cannot reach one and miss the other by construction,
   with no enum-adjacency discipline to maintain
   ([[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]). A
   `schedule` call's subject is always the caller's own label — `schedule`
   requires one, so there is no minted default that could ever disagree with
   it. `DeskAct` still names the six acts and yields both spellings, the
   rail's `verb` column and the audit's past tense.
4. **the workers that survived binding loss**, named. A `defer`red worker is
   moored by `Mooring::for_worker` onto the session root precisely so a
   foreground cancel cannot reach it, so a raise, the wall, or an `exit` takes
   the handle *binding* and leaves the work running — but not `Stopped`, which
   is no ending and keeps bindings, so a stopped call draws neither this
   sentence nor the audit one. The sentence stops at binding loss — it says
   the binding went with the unwind and so the worker cannot be `await`ed,
   which is also what keeps it from reading as a contradiction of the
   `recovery:` line's `await $h`; the exchange-boundary promise is already
   made four lines above and is not made twice. That is a different fact from
   a committed act, so it is its own sentence and the desk grows no worker
   view to hold it. Which workers are *this* dispatch's is not arithmetic
   across the seam: the dispatch's own trail carries an `Observed::Worker`
   for every birth its extent gave, and `shell_eval/report.rs`'s `render`
   joins those ids against the `` `workers `` probe, decoded by
   `Agent::probe_workers` at the run boundary — legal there on a wire seat
   exactly as on the identity seat, since the registry never crosses. A birth
   still present in the registry, running or settled-unclaimed, is named
   ([[map/core/shell-state|shell-state]]); one already claimed has left the
   registry and is nobody's orphan. The sentence names each survivor by its
   `cmd`, up to five, and counts aloud whatever it does not name — a silent
   truncation would read as "that was all of them".

The per-stage journal exists but goes unrendered: `run_shell` asks with
`trail: Some(CapturePolicy::Off)`, so every dispatch's `Report::Ran.trail`
carries a per-command record — including the command a cancel struck — and
`report::render` currently reads only the `Observed::Worker` births from it.
"This command completed, that one was cut" is the engine diagnostic's job;
nothing else in the digest walks the journal yet.

**Surface decoding.** `decode_surface` is the single decoder both delivery
regimes share: the live foreground sink `run_shell` hands `dispatch_to_report`
emits now, and the deferred sink (`deferred_sink`, installed on the transport
before each dispatch) mints identical events when a detached worker's batch
flushes. `accepted_surface` wraps it with the protected-pin guard. The
codomain is `Surface`, the shell's own closed vocabulary: five channels (the
`Pin`/`Unpin` variants are one pin channel), tried pin-first. It carries only
the structured value each channel names —
no `Card` mark tree, since that is built by whoever renders (a printer's fold
over the recorded `Display` commit) or whoever records (the commit producer's
`SurfaceBuffer`, [[map/exarch/frontend|frontend]]), never by the decoder:

- a `` `pin ``/`` `unpin `` wrapper decodes to `Surface::Pin { key, card }` /
  `Surface::Unpin { key }` — a pin is a rendered card in a slot, so its card
  is the fact itself, not a copy of one — but the host-owned `services` key is
  protected
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]):
  ordinary `surface` writes or clears there are rejected with a diagnostic
  before they reach the pin mirror or viewport; accepted pins are mirrored as
  `PinDigest`s so the [[map/exarch/agent|nudge]] layer can name pinned state
  without parsing rendered text — the read side reuses the same store rather
  than adding a second one ([[design/pins|pins]]);
- a `Map` core emits at a redirect, exec, or capability-check door decodes
  through `Observation::from_value` into `Surface::Observation`, the raw
  observation alone ([[map/exarch/io-surface|io-surface]]);
- a `` `notice `` core's ready-boundary housekeeping pushes (a worker reap, an
  idle-binding prune, a large-binding warning) decodes to `Surface::Notice`
  ([[decisions/260706_enquiry-channel|enquiry-channel]]);
- any other value tries `value_to_card` and becomes a `Surface::Card`, the one
  shape whose payload *is* a card; the closed mark set and the `value_to_card`
  / `render_card` decode-and-bind path live in [[map/exarch/cards|cards]];
- the `` `done `` completion event a detached worker flushes at the end of its
  batch decodes to `Surface::Done`;
- a value that is none of these is dropped, the same graceful degradation
  `value_to_card` gives an unknown mark.

The producer is a direct `surface` call at each kit site, with no cross-language
sentinel constant. Same-thread children inherit the sink; detached workers do
not inherit the live sink. Core buffers their `surface` calls and flushes a
settled batch to exarch's deferred sink (the inbox path); without that host
sink, the ordinary `await`/`race` path replays it into the awaiting run. Either
path keeps a bus `Emitter` clone from outliving the tool run. Across the
OS-sandbox
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
