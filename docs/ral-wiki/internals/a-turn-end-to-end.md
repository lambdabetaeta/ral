---
verified_at_commit: f91ec75
verified_at_date: 2026-06-19
anchors: [run_turn, TurnRequest, TurnReport, TurnIo, TurnState, TurnGuard, compile_turn, build_turn, run_compiled, eval_top_level, Mobile, compile_and_typecheck]
---

# A turn, end to end

**A *turn* is one top-level evaluation against a persistent `Shell`, and every
host drives the same synchronous, runtime-agnostic entry** —
`Shell::run_turn(src, TurnRequest) -> TurnReport` in `ral_core`
([`core/src/host.rs`](../../../core/src/host.rs)). No host reimplements
evaluation; each is a *request supplier* that hands `run_turn` a `TurnRequest`
and renders the flat `TurnReport` its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). **Completion is
this call returning** — never a channel disconnecting — so a detached `spawn`ed
worker (a server, a watch) cannot keep a turn from ending
([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).

**The request carries the policy axes as types, never flags.** A `TurnRequest`
is exactly the places hosts differ:

- **IO** — a `TurnIo` regime sum. `Inherit` runs on the session's ambient live
  streams (cloned, then restored); `Capture` has core mint fresh stdout/stderr
  buffers it returns in `TurnReport::Ran`'s `captured`, with stdin falling
  through to the terminal the shell holds.
- **Capabilities** — the `Capabilities` ceiling pushed for the turn's dynamic
  extent (`root()` for the REPL, a grant profile for exarch).
- **Limits** — `turn_limit` (the foreground wall) and `detached_limit` (the
  lifetime ceiling for workers the turn detaches at the durable root).
- **Surface** — an optional turn-local `SurfaceSink` (`Arc<dyn EventSink>`),
  installed only for this turn; `None` is the identity.
- **Lifecycle** — optional pre/post-exec hooks (`Box::new(())` for a host with
  none).

**The spine `run_turn` orchestrates is one straight line**, owning resources
(`Sink`, `Source`, `TurnState`, guards, buffers) while the request describes
policy:

- Mint the turn's foreground scope (a child of the shell's durable root, so a
  foreground timeout never reaches a detached worker —
  [[internals/cancellation|cancellation]]) and arm its wall, if any, **before
  compiling**, so `turn_limit` bounds the whole turn — compile and typecheck
  included, not only evaluation. `compile_turn`'s `process::clear` touches only
  the signal count, never the reaper, so the armed ceiling survives the compile.
- `compile_turn` runs `compile_and_typecheck(src, shell.session_schemes())`
  seeded from the live session ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]). A
  parse or type failure returns `TurnReport::Static { diagnostics }` at once —
  no turn state, no root context, no hooks; the host renders the diagnostics and
  treats it as status 1.
- `build_turn` materialises the `TurnState` — the whole turn-local part of a
  `Shell`, one field — seeded from the ambient session: under `Inherit` its byte
  sinks are cloned from the live streams, under `Capture` they are the fresh set
  the host reads back. The turn-local `surface` and `detached_ceiling` are
  seated on it here; the surface has no liveness role, so a clone of it can never
  define turn completion.
- `run_compiled` installs that state through a `TurnGuard` and evaluates. The
  guard swaps the new frame onto `shell.turn` and publishes the foreground and
  durable-root scopes into the signal-reachable slots. It is RAII: it restores
  the prior frame on `Drop`, so teardown survives a caught worker panic. The
  root context is installed, the pre-exec hook fires, and
  `with_capabilities(caps, |s| eval_top_level(&comp, s))` runs the annotated
  typed IR ([[internals/evaluator-machine|the machine]]) under the request's
  capability ceiling.
- `eval_top_level` installs the post-run `Mobile` on **every** outcome, so a
  `let`, `cd`, or env change persists to the next turn — the turn is a resume
  point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|turn-ends-ready]]). The post-exec hook fires and
  the guard drops.
- Back in `run_turn`, the wall is **disarmed before the cause is read**, so a
  reaper tripping in the gap between eval returning and classification cannot
  misread a turn that finished inside its budget as timed out. `timed_out` is
  then `true` only for a `Deadline` cause that genuinely elapsed; the captured
  bytes (if any) are drained, and everything flattens into
  `TurnReport::Ran { result, status, single_command, captured, timed_out }`,
  carrying the `Settled<Value>` for the host to render.

**The three hosts differ only in the request they supply.**

- The REPL's `execute_input` (`ral/src/repl/exec.rs`) supplies `script_name:
  "<stdin>"`, `Capabilities::root()`, no limits, `TurnIo::Inherit` (its stdout
  the rustyline external printer), a print-or-no-op surface, and the
  `pre-exec` / `chpwd` / `post-exec` plugin hooks; it renders with
  `print_result`, calling `run_turn` synchronously on its prompt thread.
- exarch's `run_shell` (`exarch/src/shell_eval.rs`) supplies `script_name:
  "<tool>"`, its session grant profile, a per-tool `turn_limit` and a 1 h
  `detached_limit`, `TurnIo::Capture`, and an `AgentSink` surface that decodes
  each `Value` onto its presentation bus; it renders the capped `ToolResult`.
  **The pushed grant frame *is* the sandbox** — ral's [[design/grant|grant]],
  not a source-level `grant { … }` the model could escape — which is why exarch
  needs no runtime of its own ([[design/exarch-architecture|exarch-architecture]]).
- ral's batch path (`ral/src/main.rs`) supplies `TurnIo::Inherit`, a no-op
  `EventSink`, and a `()` lifecycle — the third `run_turn` client, closing the
  one entry [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
  flagged.

The human and the model are interchangeable suppliers of top-level turns over
one persistent `Shell`.

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/cancellation|cancellation]];
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]; maps
[[map/repl|repl]], [[map/exarch|exarch]].
</content>
</invoke>
