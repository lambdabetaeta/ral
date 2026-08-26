---
verified_at_commit: be7c59e3
verified_at_date: 2026-08-26
anchors: [run, run_under, run_nested, enter, Program, register_hook, RunRequest, RunReport, Ending, RunIo, TrailScope, Mooring, IoLoan, compile_run, build_run, run_framed, run_phrases, compile_and_typecheck]
---

# A run, end to end

**A *run* is one top-level evaluation against a persistent `Shell`, and every
host starts it through a synchronous, runtime-agnostic run door** —
`Shell::run(RunRequest) -> RunReport` (or `Shell::run_under` when the host
already holds a foreground scope) in
[`core/src/run.rs`](../../../core/src/run.rs). The request's `Run`
carries a `Program` sum naming what runs: source text, or a *registered hook*
applied to first-order arguments — a `Block`/`Lambda` the host stored by name
in the session-lived hook table (`Shell::register_hook`), so the host conveys
data, never closures, across the dispatch boundary. No host reimplements
evaluation; each is a *request supplier* that hands the door a `RunRequest` and
renders `RunReport` its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). **Completion is the
door returning** — never a channel disconnecting — so a detached `spawn`ed
worker (a server, a watch) holding a clone of the surface cannot keep a run
from ending ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). The
reduction primitive behind the door is crate-private, so a host cannot start an
unframed evaluation against a stale frame
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

**There are three doors, and which one you use is decided by what you hold.**
`Shell::run` is for a host with no run in hand: its frame is minted under
`SessionState::anchor`. A host that already holds a pre-minted
`ForegroundScope` uses `Shell::run_under(&scope, req)`; the identity and wire
transports use this form so cancellation can land before a frame exists. Code
*already inside* a run — a builtin body, a lifecycle hook — uses
`Shell::run_nested(&mooring, req)`, handing it the `Mooring` it was given, so
the nested frame is a child of the enclosing run's cancel scope: the outer
run's interrupt unwinds the nest, and the outer wall reaches into it
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).

**The request carries the policy axes as types, never flags.** A `RunRequest`
is exactly the places hosts differ:

- **IO** — a `RunIo` regime sum over *byte output*. `Inherit` runs on the
  session's ambient streams (cloned, then restored); `Capture` has core mint
  fresh stdout/stderr buffers it returns in `RunReport::Ran`'s `captured`.
- **Stdin** — a `RunStdin` choice, orthogonal to the output regime: `Inherit`
  reads the session's fd 0 (terminal, pipe, or file), `Empty` installs an
  immediate-EOF source with no fall-through.
- **Terminal** — a `RequestedTerminalAccess`: `Leased` may foreground a
  terminal-bound child, `Denied` may not. `Capture` no longer implies terminal
  ownership — a piped `ral -c` is `Denied` yet `Inherit`s its stdin
  ([[decisions/260619_terminal-lease|terminal-lease]]).
- **Capabilities** — the `Capabilities` ceiling pushed for the run's dynamic
  extent (`root()` for the REPL, a grant profile for exarch).
- **Limits** — `wall` (the foreground deadline) and `deferred_lease` /
  `worker_cap` (the idle/backstop lease and the concurrency cap governing
  workers the run defers at the durable root).
- **Surface** — an optional run-local `SurfaceSink` (`Arc<dyn EventSink>`),
  installed only for this run; `None` is the identity.
- **Lifecycle** — optional pre/post-exec hooks (`Box::new(())` for a host with
  none).
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
  under, so a foreground timeout never reaches a detached worker
  ([[internals/cancellation|cancellation]]) — and arm its wall, if any,
  **before compiling**, so `wall` bounds the whole run — compile and typecheck
  included, not only evaluation. `compile_run`'s `process::clear` touches only
  the signal count, never the reaper, so the armed ceiling survives the compile.
- The source arm's `compile_run` runs `compile_and_typecheck` seeded from the
  live session's schemes ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]). A
  parse or type failure returns `RunReport::Static { diagnostics }` at once —
  no run state, no root context, no hooks; the host renders the diagnostics and
  treats it as status 1. The hook arm skips this: its program is an
  already-compiled value resolved by name in the hook table, and the hook's
  registered `DefaultPolicy` (capture, terminal authority) folds into
  the run's conditions — the hook's to decide, not the dispatching host's.
- `run_built` materialises the IO regime — `Capture` mints the buffers it reads
  back, `Inherit` leaves the ambient streams to flow — then assembles the run's
  frame **in two halves, split by mutability**. What the run fixes once (the
  `surface` sink, the deferred rail with its `deferred_lease` and `worker_cap`,
  the desk, the nursery, the foreground scope, and the run's terminal
  authority) is a `Mooring`, an owned local on `run_built`'s own Rust stack
  frame; the surface has no liveness role, so a clone of it can never define
  run completion. What genuinely changes within the run is taken on loan: the
  byte streams — the `Io` `build_run` seeds from the ambient session — and the
  two `Copy` registers the run's frame owns for its life, the root-source
  register `session.root_file` and the dispatch register
  `local.audit.call_site`.
- `run_framed` borrows the mooring, installs the run's `Io` through an
  `IoLoan`, and evaluates. The loan is RAII over that one swap: it moves the
  new streams onto `shell.io` and restores the previous ones on `Drop`, so
  teardown survives a caught worker panic. The mooring needs no guard — it
  never moved, so an outer run's is back the instant this stack frame ends, and
  the `NurseryGuard` beside it empties the nursery on the unwinding path as
  surely as on the clean one. The root context is installed, the pre-exec hook
  fires (taking `&Mooring` beside the shell, as every in-run body does), and
  `with_capabilities(caps, body)` runs the run's program under the request's
  capability ceiling — `run_phrases(&top.phrases, shell.env.clone(),
  Mode::Session, mooring, shell)` for the source arm, the in-frame
  `builtins::apply` of the resolved hook for the hook arm
  ([[internals/evaluator-machine|the machine]];
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).
- `run_phrases` writes each `let` straight into `shell.env` as its `Define`
  lands, and a `cd`, `alias`, or hook registration is an unbracketed write
  straight to `shell.context` — there is no post-run install step, so a
  halted run keeps every write that landed before the halt and the run
  remains a resume point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|exchange-ends-ready]]). Before the status is
  read, `run_framed` polls `process::check(mooring)` once more so a sticky
  cancellation cannot be absorbed by `try`; it then computes the transport
  status, fires the post-exec hook, and emits ready-boundary notices while the
  run frame and sinks are still installed. Only then does the IO guard drop.
- Back in `run_built`, the wall is **disarmed before the cause is read**, so a
  reaper tripping in the gap between eval returning and classification cannot
  misread a run that finished inside its budget as timed out. `classify_ending`
  then folds the settled `Result`, the transport status, `single_command`,
  `root`, and whether a `Deadline` cause genuinely elapsed into one
  `run::Ending` — `Settled`, `Raised`, `Walled`, `Exited`, or (unix) `Stopped`
  — so `Ok` beside a stray "timed out" flag is no longer a state the type can
  hold. `RunReport::Ran { ending, captured, trail }` carries it home; `trail`
  starts `Vec::new()` here — `run_built` has no view of the dispatch's own
  scope, only of what a body opened and closed on its own account.
- One level up, at `Shell::enter` — the durability wrapper all three run doors
  funnel through — a `Run.trail: Some` holds a `TrailScope`
  *outside* the `catch_unwind` that recovers a mid-run panic, opened before
  `dispatch` runs and closed once it returns, on every exit. That placement is
  load-bearing: the `(env, context, last_status)` checkpoint the panic arm
  rolls back does not cover `local.audit`, so only a scope the panic itself cannot skip keeps
  the trail's close law true at dispatch granularity. A clean exit's
  observations land in `RunReport::Ran.trail`; a caught panic's are drained
  and discarded — the panicked dispatch reports `Static`, never a trail.
  `RunReport::into_report` then renders the engine's `Ending` against the
  `SourceDb` — a `Raised`/`Walled` error becomes the string the host prints
  verbatim, `command_exit`/`status` computed alongside it — onto the wire's own
  `transport::Ending`, and projects each `Observation` through
  [[design/audit|`to_wire`]] onto `Report::Ran.trail: Vec<FOValue>`, unbounded
  by declaration — the wire's frame fuse is the shared backstop, as it already
  is for `captured`.

**The hosts differ only in the request they supply.**

- The REPL's `execute_input` (`ral/src/repl/exec.rs`) supplies `script_name:
  "<stdin>"`, `Capabilities::root()`, no limits, `RunIo::Inherit`,
  `RequestedTerminalAccess::Leased`, `RunStdin::Inherit`, no surface, and the
  `pre-exec` / `chpwd` / `post-exec` plugin hooks; it builds a `Program::Source`
  `Run` and drains it through `transport::dispatch_to_report` on its prompt
  thread, rendering the terminal `Report` with `print_result`. Its plugin hooks
  and prompt body (`ral/src/repl/plugin.rs`, `prompt.rs`) dispatch
  `Program::Hook` runs instead — hooks the REPL registered by name, run through
  the same frame.
- exarch's `run_shell` (`exarch/src/shell_eval.rs`) supplies `script_name:
  "<tool>"`, its session grant profile, a per-tool `wall` and a 1 h idle
  `deferred_lease` under a 24 h backstop, plus a `worker_cap` on concurrently
  running workers, `RunIo::Capture`, `RequestedTerminalAccess::Denied`,
  `RunStdin::Empty`, `trail: Some(CapturePolicy::Off)`; it builds a
  `Program::Source` `Run` and drains it through
  `transport::dispatch_to_report`, applying each live surface value onto its
  presentation bus, and renders the capped
  `ToolResult`. **The pushed grant frame *is* the sandbox** — ral's
  [[design/grant|grant]], not a source-level `grant { … }` the model could
  escape — which is why exarch needs no runtime of its own
  ([[design/exarch-architecture|exarch-architecture]]).
- ral's batch path (`ral/src/batch.rs`) supplies `RunIo::Inherit`, no surface,
  and a `()` lifecycle, with `RequestedTerminalAccess` keyed to whether it owns
  the terminal — the third source-run client of `Shell::run`, closing the one
  entry [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
  flagged.

The human and the model are interchangeable suppliers of top-level runs over
one persistent `Shell`.

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/cancellation|cancellation]];
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]; maps
[[map/repl|repl]], [[map/exarch|exarch]].
