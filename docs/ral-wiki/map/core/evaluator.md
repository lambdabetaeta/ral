---
generated_at_commit: 1f8cb95d
generated_at_date: 2026-06-15
covers_paths: [core/src/evaluator.rs, core/src/evaluator/]
---

# Map: core / evaluator

`core/src/evaluator/` runs the CBPV [[map/core/ir|IR]]. Three verbs reach outside
the module (`evaluator.rs`):

- `eval_top_level(comp, shell)` — a tool call, a REPL turn, a script line. The
  post-run `Mobile` is *installed* on the parent shell on every outcome
  (Ok / Error / Exit), because a top-level turn is a resume point.
- `apply(callee, args, shell)` — call a `Value` (closure or thunk), absorbing
  tail signals through the trampoline.
- `evaluate(comp, shell)` — a bare tail-absorbed run with no mobile contract, for
  callers already inside a session (module load, prelude bootstrap, capability
  profiles).

The result surface is `Settled<Value>` carrying `Escape` / `BodyResult`; tail
calls funnel through `absorb_tail` into the trampoline
([[decisions/260514_completion-escape-refactor|completion-escape-refactor]]). The earlier escape-propagation
bugs (try-swallows-exit, grant tail-call bypass) are fixed and regression-tested
([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]).

Internals:

- `trampoline.rs` — lands escaping `Tail` calls and hosts `apply`; `comp.rs` —
  the `Comp` step functions; `val.rs` — the side-effect-free `Val` layer
  (`eval_val`); `expr.rs` — `PrimOp` evaluation and value indexing; `call.rs` —
  the application step (`invoke`).
- `scope.rs` — dynamic frames implementing [[design/scoping|scoping]] and the five
  [[design/control-operators|control operators]]; `case.rs`, `pattern.rs` —
  matching: `assign_pattern` destructures a `Value` against a compiled
  `IrPattern` (wildcard, name, list with optional `...rest`, map with
  pre-elaborated defaults), installing bindings into the current scope; a
  mismatch is a located runtime error with an `expected … got …` message and a
  shape hint, propagating like any other failure and so catchable by `try`;
  `capture.rs` — `with_capture` for output capture; `redirect.rs` —
  the RAII redirect-frame install/unwind (`within_redirect_frame`) that wraps an
  `Exec` or scope carrying `> file` syntax, distinct from the external-command
  fd machinery in [[map/core/runtime|runtime]]'s `command/redirect.rs`.
- The command/pipeline/transport machinery — external-command dispatch,
  pipeline planning and execution, and the in-process-vs-sandboxed-child
  dispatch choice — lives in [[map/core/runtime|runtime]], which the machine
  reaches at `pipeline::run_pipeline`, `command_call::run_call`, the `command`
  redirect guards, and `transport::dispatch`, and which re-enters the machine
  only through the three verbs above
  ([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `audit.rs` — execution-tree recording.

Hot loops poll a cancellation flag cooperatively
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]). The `Shell` state these verbs
thread is [[map/core/shell-state|shell-state]].
