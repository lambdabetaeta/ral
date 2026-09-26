---
generated_at_commit: 5652477a
generated_at_date: 2026-09-26
covers_paths: [core/src/evaluator.rs, core/src/evaluator/]
---

# Map: core / evaluator

`core/src/evaluator/` runs the CBPV [[map/core/ir|IR]] as one CEK machine
(`machine.rs`) — the full narrative is
[[internals/evaluator-machine|the evaluator machine]]. **Evaluation is
entered only through framed run doors; the machine's own verbs are
crate-private.** Four reach outside the module — `evaluate`, `apply`, its
argument-free twin `force`, and `apply_handler`:

- `machine::evaluate(comp, env, mooring, shell)` (`pub(crate)`) — inject the
  computation closure ⟨M, ρ⟩ over the empty stack and step until it is empty.
  `run_phrases` (`evaluator.rs`) is the phrase-level verb a tool call, a
  REPL run, or a script line settles through: it threads a `Toplevel`'s
  `Phrase::{Define, Run}` sequence over a local `E` starting from
  `env`, running each phrase as its own closed machine, and — under
  `Mode::Session` alone — writing each landed `Define` straight into
  `shell.env` as it lands, not as a post-run install
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
  [[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]).
  Hosts call none of the four directly: they enter through the framed
  `Shell::run` door and the run spine behind it, both in `core/src/run.rs`,
  the sole way into evaluation — its `Run` (`core/src/protocol.rs`) carries
  a `Program` of source text or a registered hook. The run door checkpoints
  and rolls back `(env, context)` around every run, so a
  panicking run reports as a failed run instead of corrupting the store.
- `machine::apply(f, args, mooring, shell)` (`pub(crate)`) — the same, from a
  closed value meeting arguments (`Machine::applying` sets the first state);
  `machine::force(v, mooring, shell)` is its argument-free twin, the door of a
  hook's arity-0 entry and of a worker's body.
  A host reaches it only through the run door's hook-program arm or the
  in-frame builtin wrapper (a native applying a user function — collection
  combinators, hook dispatch — runs a *nested* machine on the host stack,
  capped by `NESTED_MACHINE_LIMIT`), so an unframed reduction is
  unconstructable.

The result surface is `Settled<Value>`, whose `Break` is a catchable `Error`
or an uncatchable `Escape`.
A tail call binds its argument into the closure's environment and puts the body
straight in focus,
so it costs no frame and depth is simply `stack.len()`; `reserve` — checked
before any pushing rule's effect — is the cap (`session.stack_limit`, the
`--recursion-limit` knob). The escape-propagation guarantees (try does not
swallow exit, grant does not bypass tail calls) are regression-tested
([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]).

A **same-thread β-step** — forcing a block or applying a lambda — evaluates
in place on the caller's `Shell`, no snapshot or restore: `force` puts a
thunk's computation and environment in focus and `beta` a λ's body, directly,
so `io`, `session`, and
`local` state are simply the one `Shell`'s and the caller's `&Mooring` is
passed along. Block and lambda entry are uniform: an unbracketed store write
in either body (`cd`, `alias`, a hook registration) persists to the caller,
no snapshot standing between the body and the store
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]).
The `Value`-level force/block split stays intact
([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]); only
their shared store-threading is made literal. The `Shell` lifetime regions
this shares belong to [[map/core/shell-state|shell-state]].

Internals:

- `machine.rs` — the whole machine: `Machine { focus: Focus, stack:
  Vec<Frame> }`, `step_eval` and its rule table `eval_rules` (one arm per
  `CompKind` — the ξ-rules, each raising with `?` so `step_eval`'s
  `stamp_focus` is their single exit), `step_return` and `step_halt` (one arm
  per `Frame` each — the two frame-table columns). `Focus::Eval` and
  `Terminal::Lambda` hold a computation closure `{ comp, env }`, a type apart
  from the thunk value's `Closure`; `Frame::To` alone carries an `Env`, and
  `Apply`, `Try` and `Guard` none. `CompKind::Capture(body)` swaps
  `shell.io.stdout` for a fresh buffer and pushes `Frame::Capture`, which
  returns the collected bytes exactly, as `Value::Bytes`; the
  checker binds that value to a fresh name and composes a `Decode` node over
  it, which — since
  the kernel's `decode` takes a value, not a computation — has no frame of
  its own: `step_eval` closes the bound variable, drops the bind's scope so
  the buffer is unshared and moves into the string uncopied, and reads it as
  the text a value boundary wants inline, no name and no frame a session can
  intercept
  ([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]],
  [[design/types|types]]). `CompKind::Bind` swaps `shell.io.stdout` to the
  ambient sink before its left computation runs (`Frame::To` carries the prior
  sink to restore), so a `Capture` node one level in only ever drains its own
  tail's bytes. `CompKind::Rec { group: Arc<GroupNode>, index }` unfolds the
  n-ary recursive group: `group.occ()` — the union of every member's
  mentions, computed once when the elaborator built the node — restricts ρ
  once, and every sibling's thunk shares that restricted ρ; the member in
  focus binds to `comp` itself, `Arc::clone`d rather
  than rebuilt, and the rest bind to a fresh `Rec` node over the same
  `group`. A recursive reference forces its name, re-entering `Rec` over the
  forced closure's own already-restricted environment, so `restrict` answers
  the identity again and every later unfold shares that one root; a group of
  one is Levy's `rec f. M` and binds its own member to the node already in
  focus, allocating nothing.
  `CompKind::Assemble` is the one rule that does O(data) work over what looks
  like value syntax — a list/record/map literal with a spread or a computed
  key, dispatching to `assemble.rs`; a plain literal stays a `Return` and
  closes for free.
  `CompKind::Exec` classifies the head through the lexical environment and
  dispatches into [[map/core/runtime|runtime]]'s `command_call`.
  `CompKind::Pipeline` launches and joins a `PipeNode` in one rule
  ([[map/core/runtime|runtime]]). `step_case` selects the arm carrying the
  scrutinee's tag, binds the payload (`Unit` for a nullary tag) to that arm's
  pattern, extending the `case`'s environment, and evaluates the arm's body there — a
  branch, not a function applied to the payload, so its store effects outlive
  the `case` as an `if` body's do
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
  The unmatched-tag error is unreachable from source — the checker has proved
  coverage — and remains for a variant that arrives untyped.
- `scope.rs` — dynamic-frame installation implementing [[design/scoping|scoping]]
  and the five [[design/control-operators|control operators]] (`WithinScope`,
  `error_record`, and `classify`, which flattens a failed `try`/`guard`/`audit`
  body into an `Outcome`). The
  `within` form installs command handlers: a per-name handler and every alias
  must be a unary lambda `{ |args| ... }`, the catch-all a binary lambda `{
  |name args| ... }`; the calling convention is fixed by the surface form and
  validated at the install boundary by `validate_handler_arity`, never sniffed
  from the runtime value
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  The handler-stack mechanics live in [[internals/handler-dispatch|handler-dispatch]].
  `WithinUndo` holds the whole cwd cell a `dir:` displaced, so a `cd` in the
  body is undone on every exit
  ([[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]]).
- `pattern.rs` — matching: `bind_pattern`/`bind_pattern_staged` destructure a
  `Value` against a compiled `IrPattern` (wildcard, name, list with optional
  `...rest`, map by key) and fold the result straight
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
- `val.rs` holds the side-effect-free `Val` layer: `close` forms a plain
  literal or a variant straight from its already-plain entries — no spread, no
  computed key, so no data-sized work — and its `Thunk` arm builds
  `⟨M, ρ|occ(M)⟩` through `Closure::new`, reading `occ` off the `ThunkNode`
  rather than walking `M`. `expr.rs` holds the primitive
  operators the elaborator's expression desugaring emits (`Negate` / `Not` /
  `Binary`) and value indexing (`Index`).
- `assemble.rs` is the `CompKind::Assemble` rule: `eval_list` splices
  `...spread` elements (cons/snoc-shaped fast paths reuse the spread's
  persistent spine; explicit beats spread, first spread wins), `eval_map`
  is shared by record and map assembly (`MapParts`/`MapPart` read either
  entry enum uniformly) — explicit entries win over spreads, a computed key
  must close to a `String`, and a duplicate discovered only here warns and
  keeps the last (SPEC §4.5); a *static* duplicate is refused at check time
  instead.
- The command/pipeline machinery — external-command dispatch,
  pipeline planning and execution, and the in-process-vs-sandboxed-child
  dispatch choice — lives in [[map/core/runtime|runtime]], which the machine
  reaches by dispatching an `Exec` node through `command_call::classify_command`
  → `run_base_frame` / `run_external`, and at
  `PipeNode::launch`/`join`; runtime re-enters the machine only through
  `machine::apply_handler` (`cfg(unix)`, `detach`'s one-shot handler call) and
  `machine::evaluate` (a stage's `(comp, env)`, from `pipeline/thread.rs` —
  a stage never rides a re-exec) — the boundary itself always evaluates its
  body in process, OS confinement being per-child in `build_command`
  ([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `audit.rs` — trail recording (`run_native`, the one audited call site for
  every native).
- `observe.rs` — `observe`, the one reader of `ir::Register`
  ([[map/core/ir|ir]]): the five pseudo-variables (`$ENV`, `$ARGS`, `$NPROC`,
  `$CWD`, `$USER`) and a `~`-path awaiting `HOME`, as a total match rather
  than a string dispatch. `$SCRIPT` is not among them — the elaborator bakes
  it to a literal, so no runtime reader exists.

Hot loops poll a cancellation flag cooperatively
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).
