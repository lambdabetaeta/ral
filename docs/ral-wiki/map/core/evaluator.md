---
generated_at_commit: 1e9fea4
generated_at_date: 2026-08-06
covers_paths: [core/src/evaluator.rs, core/src/evaluator/]
---

# Map: core / evaluator

`core/src/evaluator/` runs the CBPV [[map/core/ir|IR]] as a trampolined machine
(`evaluator.rs`). **Evaluation is entered only through framed run doors; the
machine's own verbs are crate-private.** Two reach outside the module:

- `eval_top_level(comp, mooring, shell)` (`pub(crate)`) — the run-evaluation
  verb a tool
  call, a REPL run, or a script line settles through. Hosts never call it: they
  enter through the framed `Shell::run` door and the run spine behind it, both
  in `core/src/run.rs`, the sole way into evaluation — its
  `Run` (`core/src/transport.rs`) carries a `Program` of source text or a
  registered hook
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
  [[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]).
  The post-run `Mobile` is *installed* on the parent shell on every outcome
  (Ok / Error / Exit), because a top-level run is a resume point.
- `evaluate(comp, mooring, shell)` — a bare tail-absorbed run with no mobile
  contract, for
  callers already inside a session (module load, prelude bootstrap, capability
  profiles, REPL plugin / config loading).

`apply(callee, args, mooring, shell)` is `pub(crate)`: it reduces a `Value` (closure or
thunk) applied to arguments, absorbing tail signals through the trampoline. A
host reaches it only through the run door's hook-program arm or the
in-frame builtin wrapper, so an unframed reduction is unconstructable.

The result surface is `Settled<Value>` carrying `Escape` / `BodyResult`; tail
calls funnel through `absorb_tail` into the trampoline
([[decisions/260514_completion-escape-refactor|completion-escape-refactor]]). The escape-propagation
guarantees (try does not swallow exit, grant does not bypass tail calls) are
regression-tested ([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]).

A *same-thread thunk body* — forcing a block or applying a lambda — evaluates in
place on the caller's `Shell`, sharing its `io`, session, and local state by
identity (and its `&Mooring` by the borrow itself) while swapping in only a
mobile rescoped to the closure's `captured`
environment plus a fresh frame
([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).
Block and lambda meet at one in-place routine, `Shell::with_thunk_body`, which
the kind parameterises: a `Block` enters with the caller's `$?` and folds only
`last_status` back; a `Lambda` enters with a fresh `$?` and folds back
`{last_status, cwd}`. The `Value`-level force/block split stays intact
([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]); only
their shared store-threading is made literal. The `Shell` lifetime regions this
shares belong to [[map/core/shell-state|shell-state]].

Internals:

- `trampoline.rs` loops on `Control::Tail` for O(1) tail-call space and hosts
  `apply`. Its `Value::Thunk` arm delegates to the block contract
  `eval_block`; `apply_lambda_frame` runs a lambda body in place through
  `with_thunk_body`. `comp.rs` holds the `Comp` step functions, including
  `eval_capture` — the `Capture` node's evaluation rule. `eval_capture`
  installs a buffer through `capture.rs`'s `with_capture`, strips one
  trailing newline, and decodes the bytes strictly with
  `builtins::util::decode_utf8_strict`. `comp.rs`'s `eval_seq` flushes each
  non-final part's bytes past the innermost capture buffer to the outer
  sink, so a `Capture` node drains only its tail's bytes. `val.rs` holds the
  side-effect-free `Val` layer (`eval_val`); `expr.rs` holds the primitive
  operators the elaborator's expression desugaring emits (`eval_not` /
  `eval_binary`) and value indexing (`index_value`); `call.rs` holds the
  application step (`invoke`).
- `scope.rs` — dynamic frames implementing [[design/scoping|scoping]] and the five
  [[design/control-operators|control operators]]. The `within` form installs
  command handlers: a per-name handler and every alias must be a unary lambda
  `{ |args| ... }`, the catch-all a binary lambda `{ |name args| ... }`; the
  calling convention is fixed by the surface form and validated at the install
  boundary by `validate_handler_arity`, never sniffed from the runtime value
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  The handler-stack mechanics live in [[internals/handler-dispatch|handler-dispatch]].
- `case.rs`, `pattern.rs` — matching: `assign_pattern` destructures a `Value`
  against a compiled `IrPattern` (wildcard, name, list with optional `...rest`,
  map with pre-elaborated defaults), installing bindings into the current scope.
  Without a `...rest` tail a list pattern must cover the value exactly — a longer
  list errors rather than silently dropping its extra elements. A mismatch is a
  located runtime error with an `expected … got …` message and a shape hint,
  propagating like any other failure and so catchable by `try`. Its `Name` and
  `...rest` arms — and `comp.rs`'s `eval_letrec` group reinstall and its own
  pushed fixpoint pre-install — route every scope write through
  `Shell::install_scope_binding` (`types/shell/scope.rs`), the single fused
  chokepoint that also stamps the [[map/core/shell-state|binding-lease
  ledger]] when the write lands at session scope
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]); a
  pushed fixpoint or block frame makes the predicate false with no special
  case needed at either call site.
- `capture.rs` — `with_capture` for output capture; `redirect.rs` —
  the RAII redirect-frame install/unwind (`within_redirect_frame`) that wraps an
  `Exec` or scope carrying `> file` syntax, distinct from the external-command
  fd machinery in [[map/core/runtime|runtime]]'s `command/redirect.rs`.
- The command/pipeline machinery — external-command dispatch,
  pipeline planning and execution, and the in-process-vs-sandboxed-child
  dispatch choice — lives in [[map/core/runtime|runtime]], which the machine
  reaches at `pipeline::run_pipeline`, `command_call::run_call`, and the
  `command` redirect guards, and which re-enters the machine
  only through the verbs above; the boundary verbs themselves always evaluate
  their body in process, OS confinement being per-child in `build_command`
  ([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `audit.rs` — execution-tree recording.

Hot loops poll a cancellation flag cooperatively
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).
