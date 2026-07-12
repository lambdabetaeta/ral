---
verified_at_commit: 668499f
verified_at_date: 2026-07-12
anchors: [run_turn, Program, register_hook, TurnRequest, TurnReport, TurnIo, TurnState, TurnGuard, compile_turn, build_turn, run_framed, eval_top_level, Mobile, compile_and_typecheck]
---

# A turn, end to end

**A *turn* is one top-level evaluation against a persistent `Shell`, and every
host starts it through one synchronous, runtime-agnostic door** —
`Shell::run_turn(TurnRequest) -> TurnReport` in
[`core/src/driver.rs`](../../../core/src/driver.rs). The request's `Turn`
carries a `Program` sum naming what runs: source text, or a *registered hook*
applied to first-order arguments — a `Block`/`Lambda` the host stored by name
in the session-lived hook table (`Shell::register_hook`), so the host conveys
data, never closures, across the dispatch boundary. No host reimplements
evaluation; each is a *request supplier* that hands the door a `TurnRequest` and
renders the flat `TurnReport` its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). **Completion is the
door returning** — never a channel disconnecting — so a detached `spawn`ed
worker (a server, a watch) holding a clone of the surface cannot keep a turn
from ending ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). The
reduction primitive behind the door is crate-private, so a host cannot start an
unframed evaluation against a stale frame
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

**The request carries the policy axes as types, never flags.** A `TurnRequest`
is exactly the places hosts differ:

- **IO** — a `TurnIo` regime sum over *byte output*. `Inherit` runs on the
  session's ambient streams (cloned, then restored); `Capture` has core mint
  fresh stdout/stderr buffers it returns in `TurnReport::Ran`'s `captured`.
- **Stdin** — a `TurnStdin` choice, orthogonal to the output regime: `Inherit`
  reads the session's fd 0 (terminal, pipe, or file), `Empty` installs an
  immediate-EOF source with no fall-through.
- **Terminal** — a `RequestedTerminalAccess`: `Leased` may foreground a
  terminal-bound child, `Denied` may not. `Capture` no longer implies terminal
  ownership — a piped `ral -c` is `Denied` yet `Inherit`s its stdin
  ([[decisions/260619_terminal-lease|terminal-lease]]).
- **Capabilities** — the `Capabilities` ceiling pushed for the turn's dynamic
  extent (`root()` for the REPL, a grant profile for exarch).
- **Limits** — `turn_limit` (the foreground wall) and `detached_limit` (the
  lifetime ceiling for workers the turn detaches at the durable root).
- **Surface** — an optional turn-local `SurfaceSink` (`Arc<dyn EventSink>`),
  installed only for this turn; `None` is the identity.
- **Lifecycle** — optional pre/post-exec hooks (`Box::new(())` for a host with
  none).

**The spine the door orchestrates is one straight line**, owning resources
(`Sink`, `Source`, `TurnState`, guards, buffers) while the request describes
policy. `run_turn` dispatches on the `Program` sum, and the arms differ only in
*how the program resolves* — the source arm compiles first, the hook arm looks
up the hook table; both then converge on `run_built` and the turn module's
framed scaffold:

- Mint the turn's foreground scope (a child of the shell's durable root, so a
  foreground timeout never reaches a detached worker —
  [[internals/cancellation|cancellation]]) and arm its wall, if any, **before
  compiling**, so `turn_limit` bounds the whole turn — compile and typecheck
  included, not only evaluation. `compile_turn`'s `process::clear` touches only
  the signal count, never the reaper, so the armed ceiling survives the compile.
- The source arm's `compile_turn` runs
  `compile_and_typecheck(src, shell.session_schemes())` seeded from the live
  session ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]). A
  parse or type failure returns `TurnReport::Static { diagnostics }` at once —
  no turn state, no root context, no hooks; the host renders the diagnostics and
  treats it as status 1. The hook arm skips this: its program is an
  already-compiled value resolved by name in the hook table, and the hook's
  registered `DefaultPolicy` (capture, terminal authority, budget) folds into
  the turn's conditions — the hook's to decide, not the dispatching host's.
- `run_built` materialises the IO regime — `Capture` mints the buffers it reads
  back, `Inherit` leaves the ambient streams to flow — then `build_turn`
  assembles the `TurnState`, the whole turn-local part of a `Shell` in one
  field, seeded from the ambient session. The turn-local `surface` and
  `detached_ceiling` are seated on it here; the surface has no liveness role, so
  a clone of it can never define turn completion.
- `run_framed` installs that state through a `TurnGuard` and evaluates. The
  guard swaps the new frame onto `shell.turn` and publishes the foreground and
  durable-root scopes into the signal-reachable slots. It is RAII: it restores
  the prior frame on `Drop`, so teardown survives a caught worker panic. The
  root context is installed, the pre-exec hook fires, and
  `with_capabilities(caps, body)` runs the turn's program under the request's
  capability ceiling — `eval_top_level(&comp, s)` for the source arm, the
  in-frame `builtins::apply` of the resolved hook for the hook arm
  ([[internals/evaluator-machine|the machine]];
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).
- `eval_top_level` installs the post-run `Mobile` on **every** outcome, so a
  `let`, `cd`, or env change persists to the next turn — the turn is a resume
  point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|turn-ends-ready]]). `run_framed` computes the
  transport status, fires the post-exec hook, and the guard drops.
- Back in `run_built`, the wall is **disarmed before the cause is read**, so a
  reaper tripping in the gap between eval returning and classification cannot
  misread a turn that finished inside its budget as timed out. `timed_out` is
  then `true` only for a `Deadline` cause that genuinely elapsed; the captured
  bytes (if any) are drained, and everything flattens into
  `TurnReport::Ran { result, status, single_command, captured, timed_out }`,
  carrying the `Settled<Value>` for the host to render.

**The hosts differ only in the request they supply.**

- The REPL's `execute_input` (`ral/src/repl/exec.rs`) supplies `script_name:
  "<stdin>"`, `Capabilities::root()`, no limits, `TurnIo::Inherit`,
  `RequestedTerminalAccess::Leased`, `TurnStdin::Inherit`, no surface, and the
  `pre-exec` / `chpwd` / `post-exec` plugin hooks; it calls `run_turn` with a
  `Program::Source` turn synchronously on its prompt thread and renders with
  `print_result`. Its plugin hooks and prompt body (`ral/src/repl/plugin.rs`,
  `prompt.rs`) dispatch `Program::Hook` turns instead — hooks the REPL
  registered by name, run through the same frame.
- exarch's `run_shell` (`exarch/src/shell_eval.rs`) supplies `script_name:
  "<tool>"`, its session grant profile, a per-tool `turn_limit` and a 1 h
  `detached_limit`, `TurnIo::Capture`, `RequestedTerminalAccess::Denied`,
  `TurnStdin::Empty`, and an `AgentSink` surface that decodes each `Value` onto
  its presentation bus; it calls `run_turn` with a `Program::Source` turn and
  renders the capped
  `ToolResult`. **The pushed grant frame *is* the sandbox** — ral's
  [[design/grant|grant]], not a source-level `grant { … }` the model could
  escape — which is why exarch needs no runtime of its own
  ([[design/exarch-architecture|exarch-architecture]]).
- ral's batch path (`ral/src/main.rs`) supplies `TurnIo::Inherit`, no surface,
  and a `()` lifecycle, with `RequestedTerminalAccess` keyed to whether it owns
  the terminal — the third source-turn client of `run_turn`, closing the one entry
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] flagged.

The human and the model are interchangeable suppliers of top-level turns over
one persistent `Shell`.

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/cancellation|cancellation]];
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]; maps
[[map/repl|repl]], [[map/exarch|exarch]].
</content>
</invoke>
