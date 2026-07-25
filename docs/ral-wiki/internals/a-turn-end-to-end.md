---
verified_at_commit: f7cf93a
verified_at_date: 2026-07-25
anchors: [run, Program, register_hook, RunRequest, RunReport, RunIo, RunState, RunGuard, compile_run, build_run, run_framed, eval_top_level, Mobile, compile_and_typecheck]
---

# A run, end to end

**A *run* is one top-level evaluation against a persistent `Shell`, and every
host starts it through one synchronous, runtime-agnostic door** —
`Shell::run(RunRequest) -> RunReport` in
[`core/src/run.rs`](../../../core/src/run.rs). The request's `Run`
carries a `Program` sum naming what runs: source text, or a *registered hook*
applied to first-order arguments — a `Block`/`Lambda` the host stored by name
in the session-lived hook table (`Shell::register_hook`), so the host conveys
data, never closures, across the dispatch boundary. No host reimplements
evaluation; each is a *request supplier* that hands the door a `RunRequest` and
renders the flat `RunReport` its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). **Completion is the
door returning** — never a channel disconnecting — so a detached `spawn`ed
worker (a server, a watch) holding a clone of the surface cannot keep a run
from ending ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). The
reduction primitive behind the door is crate-private, so a host cannot start an
unframed evaluation against a stale frame
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

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

**The spine the door orchestrates is one straight line**, owning resources
(`Sink`, `Source`, `RunState`, guards, buffers) while the request describes
policy. `Shell::run` dispatches on the `Program` sum, and the arms differ only
in *how the program resolves* — the source arm compiles first, the hook arm
looks up the hook table; both then converge on `run_built` and the run
module's framed scaffold:

- Mint the run's foreground scope (a child of the shell's durable root, so a
  foreground timeout never reaches a detached worker —
  [[internals/cancellation|cancellation]]) and arm its wall, if any, **before
  compiling**, so `wall` bounds the whole run — compile and typecheck
  included, not only evaluation. `compile_run`'s `process::clear` touches only
  the signal count, never the reaper, so the armed ceiling survives the compile.
- The source arm's `compile_run` runs `compile_and_typecheck` seeded from the
  live session's schemes ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]). A
  parse or type failure returns `RunReport::Static { diagnostics }` at once —
  no run state, no root context, no hooks; the host renders the diagnostics and
  treats it as status 1. The hook arm skips this: its program is an
  already-compiled value resolved by name in the hook table, and the hook's
  registered `DefaultPolicy` (capture, terminal authority, budget) folds into
  the run's conditions — the hook's to decide, not the dispatching host's.
- `run_built` materialises the IO regime — `Capture` mints the buffers it reads
  back, `Inherit` leaves the ambient streams to flow — then `build_run`
  assembles the `RunState`, the whole run-local part of a `Shell` in one
  field, seeded from the ambient session. The run-local `surface` and its
  `deferred_lease`/`worker_cap` are seated on it here; the surface has no
  liveness role, so a clone of it can never define run completion.
- `run_framed` installs that state through a `RunGuard` and evaluates. The
  guard swaps the new frame onto `shell.run` and publishes the foreground and
  durable-root scopes into the signal-reachable slots. It is RAII: it restores
  the prior frame on `Drop`, so teardown survives a caught worker panic. The
  root context is installed, the pre-exec hook fires, and
  `with_capabilities(caps, body)` runs the run's program under the request's
  capability ceiling — `eval_top_level(&comp, s)` for the source arm, the
  in-frame `builtins::apply` of the resolved hook for the hook arm
  ([[internals/evaluator-machine|the machine]];
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).
- `eval_top_level` installs the post-run `Mobile` on **every** outcome, so a
  `let`, `cd`, or env change persists to the next run — the run is a resume
  point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|exchange-ends-ready]]). `run_framed` computes
  the transport status, fires the post-exec hook, and the guard drops.
- Back in `run_built`, the wall is **disarmed before the cause is read**, so a
  reaper tripping in the gap between eval returning and classification cannot
  misread a run that finished inside its budget as timed out. `timed_out` is
  then `true` only for a `Deadline` cause that genuinely elapsed; the captured
  bytes (if any) are drained, and everything flattens into
  `RunReport::Ran { result, status, single_command, captured, timed_out }`,
  carrying the `Settled<Value>` for the host to render.

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
  `RunStdin::Empty`; it builds a `Program::Source` `Run` and drains it through
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
