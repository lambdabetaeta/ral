---
verified_at_commit: a590f4f
verified_at_date: 2026-06-18
anchors: [eval_turn, TurnFrame, TurnOutcome, TurnGuard, IoFrame, eval_top_level, Mobile, compile_and_typecheck]
---

# A turn, end to end

**A *turn* is one top-level evaluation against a persistent `Shell`, and both
binaries drive the same one** — `eval_turn(shell, src, frame) -> TurnOutcome`
in `ral_core` ([`core/src/turn.rs`](../../../core/src/turn.rs)). Neither host
reimplements evaluation; each is a *frame supplier* that hands `eval_turn` a
`TurnFrame` and renders the neutral `TurnOutcome` its own way
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

**The frame carries the policy axes as types, never flags.** A `TurnFrame` is
exactly the places the hosts differ:

- **IO** — an `IoFrame` regime sum. `Inherit` installs nothing and runs on the
  session's ambient live streams; `Capture { stdout, stderr, stdin, surface }`
  redirects the turn's streams into a fresh set the host reads back, with an
  optional surface decoder.
- **Foreground scope** — a `CancelScope` under which the turn's foreground work
  runs, always a child of the shell's durable root so a foreground timeout never
  reaches a detached worker ([[internals/cancellation|cancellation]]).
- **Capabilities** — the `Capabilities` pushed for the turn's dynamic extent.
- **Lifecycle** — optional pre/post hooks.

**The spine is one straight line** (`eval_turn`):

- `process::clear()`, then `compile_and_typecheck(src, shell.session_schemes())`
  seeded from the live session ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- A parse or type failure returns `TurnOutcome::Static { diagnostics, status: 1 }`
  at once — no turn state, no root context, no hooks; the host renders the
  diagnostics.
- Otherwise a `TurnGuard` swaps the frame's turn state onto the shell and
  publishes the foreground and durable-root scopes into the signal-reachable
  slots. It is RAII: it restores the prior turn state on `Drop`, so teardown
  survives a caught worker panic. The root context is installed, the pre-exec hook
  fires, and `with_capabilities(caps, |s| eval_top_level(&comp, s))` runs the
  annotated typed IR ([[internals/evaluator-machine|the machine]]) under the
  frame's capability ceiling.
- `eval_top_level` installs the post-run `Mobile` on **every** outcome, so a
  `let`, `cd`, or env change persists to the next turn — the turn is a resume
  point regardless of completion, error, or `exit`
  ([[invariants/turn-ends-ready|turn-ends-ready]]). The post-exec hook fires, the
  guard drops, and `TurnOutcome::Runtime { result, eval_status, single_command }`
  carries the `Settled<Value>` for the host to render.

**The two hosts differ only in the frame they supply.**

- The REPL's `execute_input` (`ral/src/repl/exec.rs`) supplies `Inherit` — its
  stdout the rustyline external printer — ambient `Capabilities::root()`, and the
  `pre-exec` / `chpwd` / `post-exec` plugin hooks; it renders with `print_result`.
- exarch's `run_shell` (`exarch/src/shell_eval.rs`) supplies `Capture` with
  `Sink::Buffer` captures and a surface decoder, its session grant profile, and a
  foreground deadline; it renders the capped `ToolResult`. **The pushed grant
  frame *is* the sandbox** — ral's [[design/grant|grant]], not a source-level
  `grant { … }` the model could escape. This is why exarch needs no runtime of its
  own ([[design/exarch-architecture|exarch-architecture]]).

The human and the model are interchangeable sources of top-level turns over one
persistent `Shell`. The `ral` batch path (`ral/src/main.rs`) still calls
`eval_top_level` directly rather than through `eval_turn` — the one remaining
un-unified entry.

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/cancellation|cancellation]]; maps [[map/repl|repl]],
[[map/exarch|exarch]].
