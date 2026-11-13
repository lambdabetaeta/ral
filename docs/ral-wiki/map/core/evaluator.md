---
generated_at_commit: 9290eb03
generated_at_date: 2026-11-13
covers_paths: [core/src/shell/evaluator.rs, core/src/shell/evaluator/]
---

# Map: core / shell / evaluator

**`core/src/shell/evaluator/` is a CEK machine over the checked CBPV IR: the
focus is a computation closure ⟨M, ρ⟩, the continuation K is a stack of
frames, and the live `Shell` is the store.** The
[[internals/evaluator-machine|machine narrative]] walks its rules;
[[invariants/ir-pure-cbpv|the IR invariant]] partitions control, primitives
and shell. Hosts enter only through the framed run in `shell/run.rs`.

## Why a CEK machine

- **Extent is structural.** A binder's scope is the environment a frame
  carries, never an ambient scope pushed and popped around calls. A `let` in
  an `if` arm scopes over the rest of that arm's block and no further.
- **Tail calls are structural.** β consumes its `Arg` frame and enters the
  body; no call frame remains, so there is no trampoline and no boundary
  that must re-absorb an escaping tail call.
- **Force is not a bracket.** Entering a thunk pushes nothing, so a store
  write in its body (`cd`) persists. `within` and `grant` are the scoped
  forms; their frames hold the undo.
- **ρ is lexical, the store is not.** ρ is a persistent map, captured by
  restriction to occurrences. `Context` (grants, handlers, env overrides,
  cwd) is store: policy code reads it directly, only frames holding undo
  change it, and no closure captures it.
- **Depth is counted in frames, before the effect.** The machine does not
  recurse on the host stack; `session.stack_limit` (default 100 000) bounds
  K and is checked before any sink swap, redirect or grant is installed.
- **Cost:** a builtin that applies a user function runs a nested machine on
  the host stack, bounded separately by `NESTED_MACHINE_LIMIT`.

Supersedes [[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]].
Refined by [[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]],
[[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]],
[[decisions/261005_ambient-reads-are-builtins|ambient-reads-are-builtins]],
[[decisions/261102_the-checked-ir-is-the-calculus|the-checked-ir-is-the-calculus]] and
[[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].

## Entry: `evaluator.rs`

- `run_phrases(top, env, mode, mooring, shell)` first readmits `top.admits`
  against the supplied `env` (private `readmit`, under `shell.fixings()`). A
  refusal is located at the mismatch witness and named `$name`; no phrase
  runs.
- It then runs each `Phrase` as its own machine. A landed `Define` extends
  `Ran::env` at once, so earlier definitions survive a later phrase's
  failure; a non-final phrase's error records the abandoned tail.
- `Mode::Session` also writes each binding to `shell.env` and the lease
  ledger (`Shell::note_define`); `Mode::Local` does neither.
  `shell::load::{run_module, evaluate_closed, extend_session}` run over it.
- `evaluator::apply` admits a thunk or builtin and calls `machine::apply`;
  builtins taking functions and session hooks use it.

## Machine: `machine.rs`

- **Configuration.** `Machine { cclos, stk }` is ⟨(M, ρ), K⟩.
  `CompClosure = Settled<(Arc<Comp>, Env)>` is ⟨M, ρ⟩, with `Err(e)` read as
  ⟨raise e, ∅⟩. `Stack` is K: `frames`, `call_site_frames`, and the run's
  `mooring`.
- **Step.** `Stack::step` maps one `CompClosure` to the next, borrowing only
  K, so `Machine::run` still owns every frame after a panic and abandons them
  top-down; there is no placeholder state. An error pops a frame into
  `unwind`; a λ pops a frame into `resume`; any other M goes to `eval`.
- **Returns are fused.** Rules yield `Next`: a closure, or a `ValClos` for the
  frame below. `Stack::settle` hands a value down through frames until one
  yields a closure. There is no value register and no allocated `return V`.
  `boxed` closes a λ meeting a value-receiving frame, or the empty stack, as
  `ValClos::thunk`.
- **Application.** `App` pushes one `Frame::Arg` per argument, first on top,
  then focuses the head. β happens at `Frame::Arg`: ⟨λx. M, ρ⟩ meeting
  `Arg(V)` is ⟨M, ρ[x ↦ V]⟩. A value meeting `Arg` is an invariant failure,
  never an implicit force. `push_args` reserves all n frames at once.
- **Builtins.** `ValClos::into_computation` forces a builtin to its cached
  η-expansion (`BuiltinEntry::eta`), a λ-chain ending in δ
  (`CompKind::Builtin`), which reads the entry and arguments from ρ. Arity
  zero enters δ at once; under-application leaves an ordinary λ residual.
  `Stack::call` runs a builtin given exactly its arity directly, skipping η.
- **Literal force.** `force_val` runs a literal block in place, without
  closing it, for `if`, `case`, `force`, `within`, `grant`, `audit`,
  `gather` and `try`'s body. `case` first pushes one `Arg` holding the
  payload (`Unit` for a bare tag).
- **Sequencing.** Every `let` runs its RHS. `Bind` pushes `Frame::To`, which
  destructures the boxed result with `pattern::bind`; `Seq` pushes
  `Frame::Seq`. These are the only frames that carry an `Env`.
- **Recursion.** `Rec` unfolds one member of a shared `GroupNode`; every
  sibling closes over ρ restricted to the group's occurrences, and recursive
  references re-enter through force without rewriting bodies.
- **Provenance.** `eval` runs each named operation under `Shell::at_site`
  and locates its halts there; `Exec`, `App` and `Force` instead enter a
  `Frame::CallSite`, which restores the displaced site on return, halt or
  panic. Adjacent call sites coalesce and cost no stack budget. A spanless
  computation (η, decoded IR) takes the innermost call site's span
  (`or_call_site`); code is never rewritten to carry a span. See
  [[decisions/261105_the-error-record|the error record]].
- **Stack cap.** Every computational push goes through a `Slot` from
  `reserve`/`reserve_n`, checked against `session.stack_limit` first.
  `Machine::run` refuses past `NESTED_MACHINE_LIMIT` nested machines.
- **Undo.** `resume` and `unwind` are each frame's return and halt rules;
  `Frame::abandon` is its panic rule. Only `Seq` and a written `let` mark
  the abandoned tail; compiler-written binds do not.
- **`try`.** `Frame::Try` passes values; on an error it enters a `try` call
  site and calls the handler with the error as plain record data. Escapes pass
  through.
- **`guard`.** Entry checks room for three frames, polls, shields the scope
  and pushes `Frame::Acquire`. Acquire's return restores the scope, pushes
  `Frame::Guard` and calls the body with the resource under a `guard` call
  site. `guard_release` saves the outcome in `Frame::Cleanup`, shields again
  and calls release with the resource and `exit_case` under a `guard` call
  site; release's return restores the outcome, its halt overrides it. A
  halting acquire runs no release. Shields change `shell.scope`, not the
  cradle. See [[decisions/261111_a-guard-acquires|a-guard-acquires]].
- **`capture`.** `Frame::Capture` swaps stdout for a buffer. Return restores
  the sink and yields the bytes, or refuses on overflow; a halt restores the
  sink, drops the buffer, and adds its byte count to an error's
  `unwound.discarded`.
- **`use`.** `Use(module)` runs the checked module through
  `shell::load::run_module` and yields its public bindings as a record.
- **Shell forms.** `Exec` resolves its head (`Resolution::of`) to a binding,
  a handler (pushing `Frame::Unmask`), a base handler or an external
  command; a boundary builtin runs saturated through `run_boundary` with its
  checked `Site`. `Pipeline` runs a `PipeNode` to a value; each ral stage
  starts a fresh machine through `STAGE_EVAL`. `Spawn` calls
  `concurrency::spawn_form`; `Frame::Gather` owns a `BroodGuard`, installs a
  cradle over the current scope, and restores the displaced one on exit. See
  [[internals/pipeline-execution|pipelines]] and
  [[invariants/a-worker-ends-with-its-children|worker scope]].
- **Entry points.** `machine::evaluate` runs ⟨(M, ρ), ε⟩. `machine::apply`
  runs a builtin given exactly its arity without building a machine;
  otherwise it pushes the arguments, forces the head and runs.

## Values and primitives

- `ir/value.rs`, `runtime/close.rs`: `ValClos::close` closes one layer of
  syntax over ρ restricted to its occurrences, pulling Σ's answers for
  missing direct names; `ValClos::thunk` does the same for a computation.
  `VClosRef::view` reads a head constructor only; lists and maps expose
  `ListRef` / `MapRef`. See
  [[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].
- `expr.rs`: `eval_prim` interprets every `Prim` on values. `field`,
  `position` and `lookup` read `Key::{Label, Position, Data}`. `Decode`
  drops ρ before taking the bytes, strips one trailing terminator and
  decodes UTF-8 strictly.
- `assemble.rs`: `eval_list`, `eval_update` and `eval_map` build collections
  from already-run operands, calling `ListRef`'s cons, snoc and concat. A repeated computed map key fails here; known
  duplicates are refused statically.
- `pattern.rs`: `destructure` stages every binding of a
  `Pattern::{Wildcard, Name, List, Record}` before any is installed; a list
  pattern without rest requires exact length. Mismatches are catchable.

## Shell interpretation

- `scope.rs`: `WithinScope::parse` reads `within`'s option record (`dir`,
  `env`, `handler`, `handlers`); naming either handler key installs a
  handler frame, even for an empty map. `WithinScope::enter` returns the
  `WithinUndo` that `Frame::Within` applies on exit. Handler dispatch is
  [[internals/handler-dispatch|deep and self-masking]].
- `Grant` pushes a capability layer; `Frame::Grant` removes it by position.
  `Audit` opens a trail scope; `Frame::Audit` turns a value or error into a
  report and passes escapes.
- `call.rs`: `run_native` runs a builtin body inside `BuiltinEntry::framed`;
  the body may re-enter the machine.
- `Tilde` reads `HOME` through `Context::require_home`; other ambient reads
  are builtins.

See [[map/core/shell-state|shell state]], [[map/core/runtime|runtime]],
[[map/core/typecheck|typecheck]].
