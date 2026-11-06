---
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
verified_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
verified_at_date: 2026-11-06
anchors: [run, run_under, Program, register_hook, RunRequest, RunReport, Ending, RunIo, TrailScope, Mooring, IoLoan, compile_run, build_run, run_framed, run_phrases, compile_and_typecheck, Agent::ral, dispatch_to_report, Scopes::open, Scope::id]
---

# A run, end to end

**A *run* is one top-level evaluation against a persistent `Shell`, and every
host starts it through a synchronous, runtime-agnostic run door** —
`Shell::run(RunRequest) -> RunReport` (or `Shell::run_under` when the host
already holds a foreground scope) in
[`core/src/shell/run.rs`](../../../core/src/shell/run.rs). The request's `Run`
carries a `Program` sum naming what runs: source text, or a *registered hook*
applied to first-order arguments — a block, lambda, or builtin registered by
name in the session-lived hook table (`Shell::register_hook`), so the host
conveys data, never closures, across the dispatch boundary. No host
reimplements evaluation, and no front-end calls the door itself: each is a
*run supplier* that dispatches a `Run` through a transport, whose engine
(`Engine::run`) enters the door and projects the `RunReport` onto the wire's
`Report` for the front-end to render its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). **Completion is the
door returning** — never a channel disconnecting — so a `spawn`ed
worker (a server, a watch) holding a clone of the surface cannot itself keep
a run from ending; only `Run.last` explicitly requests the session-worker wait
([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). The
reduction primitive behind the door is crate-private, so a host cannot start an
unframed evaluation against a stale frame
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

**There are two doors, and which one you use is decided by what you hold.**
`Shell::run` is for a host with no run in hand: its frame is minted under
`SessionState::anchor`. A host that already holds a pre-minted dispatch scope (`Scopes::open`'s view) uses `Shell::run_under(&scope, req)`; the identity and wire
transports use this form so cancellation can land before a frame exists. Code
*already inside* a run — a builtin body — opens no second run: it applies a
function under the `Mooring` it was given, so the outer run's interrupt and
wall reach whatever it calls
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).

**The request carries the run policy.** A `RunRequest`
is exactly the places hosts differ:

- **IO** — a `RunIo` regime sum over *byte output*. `Inherit` runs on the
  session's ambient streams (cloned, then restored); `Capture` has core mint
  fresh stdout/stderr buffers it returns in `RunReport::Ran`'s `captured`.
- **Stdin** — a `RunStdin` choice, orthogonal to the output regime: `Inherit`
  reads the session's fd 0 (terminal, pipe, or file), `Empty` installs an
  immediate-EOF source with no fall-through.
- **Terminal** — a `RequestedTerminalAccess`: `Leased` may foreground a
  terminal-bound child, `Denied` may not. `Capture` does not imply terminal
  ownership — a piped `ral -c` is `Denied` yet `Inherit`s its stdin
  ([[decisions/260619_terminal-lease|terminal-lease]]).
- **Limits** — `wall` (the foreground deadline), `worker_cap` (the cap on
  concurrently running workers), `cleanup_grace` (how long a halting worker or
  combinator waits for the children it cancels), and `worker_title` (whether
  a `spawn` must carry a title).
- **Surface** — an optional run-local `SurfaceSink` (`Arc<dyn EventSink>`),
  installed only for this run; `None` is the identity.
- **Last run** — `Run.last`: when the body returns, claim and wait for
  still-unclaimed session workers in birth order, replay stdout then stderr
  and discard every ending. This lifetime join never adopts a loose
  worker's failure. The wait stays inside the run's frame, wall and trail;
  cancellation or a replay failure can still halt it.
- **Trail** — `trail: Option<CapturePolicy>` ([[design/audit|audit]]). `Some`
  delimits this dispatch's own extent as an audit scope and carries it home on
  `RunReport::Ran`; `None` neither opens a scope nor collects one.

**The spine the run doors orchestrate is one straight line**, owning resources
(`Sink`, `Source`, the run's frame, guards, buffers) while the request describes
policy. The run doors dispatch on the `Program` sum, and the arms differ only
in *how the program resolves* — the source arm compiles first, the hook arm
looks up the hook table; both then converge on `run_built` and the run
module's framed scaffold:

- Mint the run's foreground scope — a child of the scope the door was entered
  under, so a foreground timeout never reaches a loose session worker
  ([[internals/cancellation|cancellation]]) — and arm its wall, if any,
  **before compiling**, so `wall` bounds the whole run — compile and typecheck
  included, not only evaluation. `compile_run`'s `process::clear` touches only
  the signal count, never the reaper, so the armed ceiling survives the compile.
- The source arm's `compile_run` runs `compile_and_typecheck` seeded from the
  live session's schemes ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]). A
  parse or type failure returns `RunReport::Static { diagnostics }` at once —
  no run state, no root context. The diagnostic carries the `Source`
  its carets point into rather than registering it: a run that failed to
  compile leaves no live span, so its text has no business in a registry that
  is append-only precisely because live spans index it
  ([[decisions/260902_static-diagnostics-render-at-the-seam|static-diagnostics-render-at-the-seam]]).
  The hook arm skips this: its program is an
  already-compiled value resolved by name in the hook table, and the hook's
  registered `DefaultPolicy` (capture, terminal authority, and `aside` —
  run on a `join_session` sibling, so nothing flows back) folds into the run's
  conditions — the hook's to decide, not the dispatching host's. A fault in
  it is labelled by `HookName::fault_label` (`plugin 'p' hook 'h'`,
  `prompt`), and a hook run is no ready boundary: only a source run emits
  the boundary notices.
- `run_built` materialises the IO regime — `Capture` mints the buffers it reads
  back, `Inherit` leaves the ambient streams to flow — then assembles the run's
  frame **in two halves, split by mutability**. What the run fixes once (the
  `surface` sink, the deferred rail with its `worker_cap`, `cleanup_grace` and `worker_title`,
  the desk, the `Fork` arm, the foreground scope, and the run's terminal
  authority) is a `Mooring`, an owned local on `run_built`'s own Rust stack
  frame; the surface has no liveness role, so a clone of it can never define
  run completion. What genuinely changes within the run is taken on loan: the
  byte streams — the `Io` `build_run` seeds from the ambient session — and the
  two registers the run's frame owns for its life, the root-source
  register `session.root_file` and the call-site register in
  `local.audit`.
- `run_framed` borrows the mooring, installs the run's `Io` through an
  `IoLoan`, and evaluates. The loan is RAII over that one swap: it moves the
  new streams onto `shell.io` and restores the previous ones on `Drop`, so
  teardown survives a caught worker panic. The mooring needs no guard — it
  never moved, so an outer run's is back the instant this stack frame ends,
  and the `NurseryGuard` beside it empties the nursery on the unwinding path
  as surely as on the clean one. A source run registers its root
  (`register_root`) and installs a base `Prov` naming its whole-program span;
  a hook registers no text but installs its resolved name and origin at 1:1. Workers inherit
  the parent's register at spawn. The body runs under the session's stack. `audit_deputy_prefixes` reports its flagged
  prefixes to the run's trail before evaluation — `run_phrases(&top.phrases,
  shell.env.clone(), Mode::Session, mooring, shell)` for the source arm, the
  in-frame `shell::evaluator::apply` of the resolved hook for the hook arm
  ([[internals/evaluator-machine|the machine]];
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).
- `run_phrases` writes each `let` straight into `shell.env` as its `Define`
  lands, and a `cd`, `alias`, or hook registration is an unbracketed write
  straight to `shell.context` — there is no post-run install step, so a
  halted run keeps every write that landed before the halt and the run
  remains a resume point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|exchange-ends-ready]]). After any `Run.last`
  return-wait, `run_framed` disarms the wall (waiting on a firing already claimed)
  and makes the **closing poll**, `final_poll` on the run's node, so a stop
  that landed after the body's last poll — a key a terminal loan heard back
  and struck on the node included — is not lost; a stop is an escape no `try`
  absorbs anyway. It emits ready-boundary notices while the run frame and
  sinks are still installed. Before the IO guard restores the caller, any
  unlocated error is completed from the run's own total register.
- Back in `run_built`, `End::settled` folds the machine result into
  `Ending<Value, Error, Stop>`: `Ok`, `Err`, `Exited` or `Stopped`.
  A wall is a `TimedOut` stop, not a separate flag.
  `RunReport::Ran { ending, base, compact, captured, trail }` carries it home;
  `base`, the run's origin at 1:1 that `run_under` resolves once, sites a
  data-boundary refusal after the register is restored; `trail`
  starts `Vec::new()` here — `run_built` has no view of the dispatch's own
  scope, only of what a body opened and closed on its own account.
- One level up, at `Shell::enter` — the durability wrapper all three run doors
  funnel through — a `Run.trail: Some` holds a `TrailScope`
  *outside* the `catch_unwind` that recovers a mid-run panic, opened before
  `dispatch` runs and closed once it returns, on every exit. That placement is
  load-bearing: the `(env, context)` checkpoint the panic arm
  rolls back does not cover `local.audit`, so only a scope the panic itself cannot skip keeps
  the trail's close law true at dispatch granularity. A clean exit's
  observations land in `RunReport::Ran.trail`; a caught panic's are drained
  and discarded — the panicked dispatch reports `Ran` with `End::Panic`,
  status 101, no compact root, capture or trail.
  `RunReport::into_report` calls `End::render` against the shell's sources,
  borrowing its actual session exit-hints table;
  errors, stops and panics share the `Diagnosis` renderer. The wire alias is
  `Ending<FOValue, ErrorRecord, CancelCause>`: a value becomes first-order data,
  an error its closed `[reason, frame, site]` record, and a `Stop` its cause.
  A non-data value becomes `Err`, code 1, completed with the retained base
  even after the caller's frame is restored. `Reason` is shared by runtime
  error and record; code and description are projections
  ([[decisions/261105_the-error-record|the error record]]).
  `Report::Ran(Ran { ending, rendered, single_command, captured, trail })` keeps
  run context outside the cases; `Report::code()` is the host's `u8` status.
  A `Static` renders through `StaticDiagnostics::render` (2 parse, 1 type);
  `Report::Faulted` is only a lost report, not a known panic ending. The seam
  projects each `Observation` through
  [[design/audit|`encode`]] onto `Report::Ran.trail: Vec<FOValue>`, unbounded
  by declaration — the wire's frame fuse is the shared backstop, as it already
  is for `captured`.

**The hosts differ only in the request they supply.**

- The REPL's `exec::step` (`ral/src/repl/exec.rs`) supplies `origin:
  SourceOrigin::named("<stdin>")`, `GrantStack::root()`, no limits, `RunIo::Inherit`,
  `RequestedTerminalAccess::Leased`, `RunStdin::Inherit`; it builds a
  `Program::Source` `Run` and drains it through `carrier::dispatch_to_report(..., None)`
  on its `IdentityTransport` with the REPL's own `Host` (`ReplHost`),
  rendering the terminal `Report` with `print_result` — an `Err(Severed)`
  prints the cause and ends the session. The `pre-exec`, `chpwd` and
  `post-exec` plugin hooks are dispatches of their own around it, as are the
  prompt body and every other hook (`ral/src/repl/plugin.rs`, `prompt.rs`):
  `Program::Hook` runs of hooks registered by name, through the same door
  ([[map/repl/loop|loop]]).
- exarch's `run_shell` (`exarch/src/shell_eval.rs`) supplies a virtual
  `origin` naming the call's turn (`turn N`), its session grant profile, a
  per-tool `wall`, a `worker_cap` on concurrently running workers, a 2 s
  `cleanup_grace`, a required `worker_title`, `RunIo::Capture`,
  `RequestedTerminalAccess::Denied`, `RunStdin::Empty`, `trail: None`;
  it builds a `Program::Source` `Run` and drains it through
  `carrier::dispatch_to_report(..., Some(exchange))` against its call's `RunHost` —
  one `Host` pairing the enquiry desk with the applier that renders each
  live surface value onto the presentation bus — and hands back the
  `Result<Report, Severed>` itself; `Agent::ral` renders the capped
  `ToolResult`, or the one sentence `agent::EngineLost` renders for a
  severance ([[map/exarch/agent|agent]]). **The session's host-installed
  layers *are* the sandbox** — ral's [[design/grant|grant]], not a
  source-level `grant { … }` the model could escape — which is why exarch
  needs no evaluator of its own
  ([[design/exarch-architecture|exarch-architecture]]). Its owner passes the
  explicit exchange scope: identity dispatches mint beneath it, while the
  wire drain polls it every `POLL_SLICE` and strikes only this dispatch id.
  The dispatcher also checks before sending; cancellation already in force
  opens the run born cancelled. The engine root still dominates termination:
  `Scopes::end` strikes both root and current dispatch, even when its parent
  is an exarch exchange. Cancelling threads never write to an engine.
  A foreground shell call borrows the agent body through `Loanable::lend`;
  it returns on unwind too. Deferred worker batches carry engine generation,
  surviving Stop/reply and rejected after clear replaces the engine. Loose
  workers belong to the engine. Jobs are recorded busy periods for every
  agent; Synod gates its root across baseline and closing walks. Stop recalls
  human text and cancels only exchange, leaving helpers working
  ([[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]]).

- ral's batch path (`ral/src/batch.rs`) boots an `IdentityTransport` from the
  `batch` installer, dispatches the `_ral-boot` hook, then the script as one
  `Program::Source` run with `last: true`, `RunIo::Inherit` and
  `RequestedTerminalAccess` keyed to whether it owns the terminal — the same
  door, the same ending law, and the same typecheck as the REPL
  ([[map/repl/startup|startup]]). Return waits for loose workers and replays
  their bytes without adopting their endings; halt skips the wait and session
  teardown takes them down. Batch silently consumes their `done` events.
  REPL lines and exarch calls leave `last` false; their session ends are halts
  ([[design/endings|endings]]).

The human and the model are interchangeable suppliers of top-level runs over
one persistent `Shell`.

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/cancellation|cancellation]];
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]; maps
[[map/repl|repl]], [[map/exarch|exarch]].
