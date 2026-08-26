---
generated_at_commit: 68f1964e
generated_at_date: 2026-08-26
covers_paths: [core/src/evaluator.rs, core/src/evaluator/]
---

# Map: core / evaluator

`core/src/evaluator/` runs the CBPV [[map/core/ir|IR]] as one CEK machine
(`machine.rs`) — the full narrative is
[[internals/evaluator-machine|the evaluator machine]]. **Evaluation is
entered only through framed run doors; the machine's own verbs are
crate-private.** Two reach outside the module:

- `machine::evaluate(closure, mooring, shell)` (`pub(crate)`) — inject
  `Closure { comp, env }` over the empty stack and step until it is empty.
  `run_phrases` (`evaluator.rs`) is the phrase-level verb a tool call, a
  REPL run, or a script line settles through: it threads a `Toplevel`'s
  `Phrase::{Define, Source, Run}` sequence over a local `E` starting from
  `env`, running each phrase as its own closed machine, and — under
  `Mode::Session` alone — writing each landed `Define` straight into
  `shell.env` as it lands, not as a post-run install
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
  [[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]).
  Hosts never call either directly: they enter through the framed
  `Shell::run` door and the run spine behind it, both in `core/src/run.rs`,
  the sole way into evaluation — its `Run` (`core/src/transport.rs`) carries
  a `Program` of source text or a registered hook. The run door checkpoints
  and rolls back `(env, context, last_status)` around every run, so a
  panicking run reports as a failed run instead of corrupting the store.
- `machine::apply(f, args, mooring, shell)` (`pub(crate)`) — the same, from a
  closed value meeting arguments (`Machine::applying` sets the first state).
  A host reaches it only through the run door's hook-program arm or the
  in-frame builtin wrapper (a native applying a user function — collection
  combinators, hook dispatch, pattern defaults — runs a *nested* machine on
  the host stack, capped by `NESTED_MACHINE_LIMIT`), so an unframed reduction
  is unconstructable.

The result surface is `Settled<Value>` carrying `Escape` / `BodyResult`.
A tail call binds its argument into the closure's environment and puts the body
straight in focus,
so it costs no frame and depth is simply `stack.len()`; `reserve` — checked
before any pushing rule's effect — is the cap (`session.stack_limit`, the
`--recursion-limit` knob). The escape-propagation guarantees (try does not
swallow exit, grant does not bypass tail calls) are regression-tested
([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]).

A **same-thread β-step** — forcing a block or applying a lambda — evaluates
in place on the caller's `Shell`, no snapshot or restore: `force`/`beta` in
`machine.rs` step the body's `Closure` directly, so `io`, `session`, and
`local` state are simply the one `Shell`'s and the caller's `&Mooring` is
passed along. `beta` resets `$?` to 0 before a lambda's body runs; `force`
does not, so a block sees the caller's `$?`. Beyond that one reset, block and
lambda are uniform: an unbracketed store write in either body (`cd`,
`alias`, a hook registration) persists to the caller, no snapshot standing
between the body and the store
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]).
The `Value`-level force/block split stays intact
([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]); only
their shared store-threading is made literal. The `Shell` lifetime regions
this shares belong to [[map/core/shell-state|shell-state]].

Internals:

- `machine.rs` — the whole machine: `Machine { focus: Focus, stack:
  Vec<Frame> }`, `step_eval` (one arm per `CompKind` — the ξ-rules),
  `step_return` and `step_halt` (one arm per `Frame` each — the two frame-table
  columns). `CompKind::Capture(body)` installs a buffer through
  `evaluator::capture`'s `with_capture` and returns the collected bytes
  exactly, as `Value::Bytes`, under `Frame::Capture`; the checker composes a
  `Decode` node over it, evaluated under `Frame::Decode`, which moves those
  bytes out and reads them as the text a value boundary wants — no name, no
  binder, nothing a session can intercept
  ([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]],
  [[design/types|types]]). `CompKind::Bind` swaps `shell.io.stdout` to the
  ambient sink before its left computation runs (`Frame::To` carries the prior
  sink to restore), so a `Capture` node one level in only ever drains its own
  tail's bytes — the literal continuation of what a separate `eval_seq` used
  to flush. `CompKind::Rec { group, index }` unfolds the n-ary recursive
  group: every member's name binds to the thunk of its own projection: a
  recursive reference forces its name, re-entering `Rec` and re-extending
  from the outer environment; a group of one is Levy's `rec f. M`.
  `CompKind::Exec` classifies the head through the lexical environment and
  dispatches into [[map/core/runtime|runtime]]'s `command_call`.
  `CompKind::Pipeline` pushes `Frame::Pipe(Box<PipeNode>)`
  ([[map/core/runtime|runtime]]). `step_case` selects the arm carrying the
  scrutinee's tag, binds the payload (`Unit` for a nullary tag) to that arm's
  pattern in a fresh environment, and evaluates the arm's body there — a
  branch, not a function applied to the payload, so its store effects outlive
  the `case` as an `if` body's do
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
  The unmatched-tag error is unreachable from source — the checker has proved
  coverage — and remains for a variant that arrives untyped.
- `scope.rs` — dynamic-frame installation implementing [[design/scoping|scoping]]
  and the five [[design/control-operators|control operators]] (`WithinScope`,
  `error_record`, the `try`/`guard` outcome classifier `Outcome`). The
  `within` form installs command handlers: a per-name handler and every alias
  must be a unary lambda `{ |args| ... }`, the catch-all a binary lambda `{
  |name args| ... }`; the calling convention is fixed by the surface form and
  validated at the install boundary by `validate_handler_arity`, never sniffed
  from the runtime value
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  The handler-stack mechanics live in [[internals/handler-dispatch|handler-dispatch]].
- `pattern.rs` — matching: `bind_pattern`/`bind_pattern_staged` destructure a
  `Value` against a compiled `IrPattern` (wildcard, name, list with optional
  `...rest`, map with pre-elaborated defaults) and fold the result straight
  into an `Env`. Without a `...rest` tail a list pattern must cover the value
  exactly — a longer list errors rather than silently dropping its extra
  elements. A mismatch is a located runtime error with an `expected … got …`
  message and a shape hint, propagating like any other failure and so
  catchable by `try`. All-or-nothing: `stage_pattern` collects every binding
  first, so a pattern that fails partway leaves the `Env` it was given
  untouched. `bind_pattern_staged`'s `observe` callback is how
  `evaluator.rs`'s `run_phrase_define` reaches [[map/core/shell-state|`Shell::note_define`]]
  beside each name's install — under `Mode::Session` alone, so only a
  session-scope write stamps the binding-lease ledger; a block, lambda, or
  `Rec` group's fixpoint pre-install binds unobserved
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]).
- `capture.rs` — `with_capture` for output capture; `redirect.rs` — the
  redirect-frame open/route/restore lifecycle (`RedirectState`), entered
  directly by `machine.rs`'s `Frame::Redirect` and by `with_redirects` for a
  base-frame native's synchronous call, distinct from the external-command
  fd machinery in [[map/core/runtime|runtime]]'s `command/redirect.rs`.
- `val.rs` holds the side-effect-free `Val` layer (`close`); `expr.rs` holds
  the primitive operators the elaborator's expression desugaring emits
  (`Negate` / `Not` / `Binary`) and value indexing (`Index`).
- The command/pipeline machinery — external-command dispatch,
  pipeline planning and execution, and the in-process-vs-sandboxed-child
  dispatch choice — lives in [[map/core/runtime|runtime]], which the machine
  reaches by dispatching an `Exec` node through `command_call::classify_command`
  → `run_base_frame` / `run_handler` / `run_external`, and at `Frame::Pipe`/
  `PipeNode::launch`; runtime re-enters the machine only through
  `machine::apply` (a handler/alias thunk) and `machine::evaluate` (a re-exec'd
  stage's closure) — the boundary itself always evaluates its body in
  process, OS confinement being per-child in `build_command`
  ([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `audit.rs` — execution-tree recording (`run_native`, the one audited call
  site for every native).
- `observe.rs` — `observe`, the one reader of `ir::Register`
  ([[map/core/ir|ir]]): the five pseudo-variables (`$ENV`, `$ARGS`, `$NPROC`,
  `$CWD`, `$USER`) and a `~`-path awaiting `HOME`, as a total match rather
  than a string dispatch. `$SCRIPT` is not among them — the elaborator bakes
  it to a literal, so no runtime reader exists.

Hot loops poll a cancellation flag cooperatively
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).
