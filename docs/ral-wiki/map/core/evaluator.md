---
generated_at_commit: c0905f5e
generated_at_date: 2026-11-12
covers_paths: [core/src/shell/evaluator.rs, core/src/shell/evaluator/]
---

# Map: core / shell / evaluator

**`core/src/shell/evaluator/` is the CEK machine for the checked CBPV IR.**
The [[internals/evaluator-machine|machine narrative]] describes its rules;
[[invariants/ir-pure-cbpv|the IR invariant]] partitions control, primitives
and shell. Hosts enter only through the framed run in `shell/run.rs`.

## Entry

- `machine::evaluate(comp, env, mooring, shell)` injects ⟨M, ρ⟩ over an
  empty stack. `machine::apply(f, args, …)` seeds `call`: an exact-arity
  builtin runs directly; otherwise arguments are pushed and the value forced.
  Both are crate-private and return `Settled<ValClos>`.
- `run_phrases(top: &Toplevel, env: Env, …)` (`shell/evaluator.rs`)
  readmits `top.admits` against the supplied `env` before the first phrase,
  in every mode, including loaded files. Private `readmit` uses that `Env`
  and `shell.fixings()`, not `shell.env`. A refusal returns `Ran` with the
  supplied environment and the error; no phrase runs.
  Readmission claims provenance at the mismatch witness, naming the use
  that imposed the type; loaded-file errors retain the file's own location
  and, for `use`, its loader context.
- It then runs `Phrase::{Define, Run}` in order. A landed `Define`
  immediately extends the environment; under `Mode::Session` it also updates
  `shell.env` and the lease ledger. Earlier definitions survive a later
  phrase's failure.
- `Mode::Session` leases and writes back; `Mode::Local` does neither.
  `shell::load::run_module` and `evaluate_closed` share execution under
  `Env::new()` with Σ lookup. `extend_session` is the rc and agent-library
  door. A linked module has no session imports to readmit.
- Higher-order natives re-enter through `machine::apply`; session hooks
  through `evaluator::apply`. `NESTED_MACHINE_LIMIT` bounds host-stack
  nesting independently of the machine's frame cap.

## Machine: `machine.rs`

- `State = Settled<(Arc<Comp>, Env)>` holds only a computation closure or
  `Err(Break)`. `Next` is a closure or `ValClos`, a transient rule result.
  `settle` hands values to frames in the same step, never storing a value
  state or constructing `return V` IR. `boxed` closes a residual λ through
  `ValClos::thunk` at value demands and run end.
- `eval_rules` interprets `CompKind`; `resume` and `unwind` interpret frames.
  Rules return `Settled<Next>`; `step` pops a frame for a λ or break and
  settles the result. Raising points use visibility-checked `Break::locate`.
  `Exec`/`App` claim their innermost dispatch/admission failures; builtin δ
  uses `run_native`'s own provenance.
- `App` pushes one `Frame::Arg` per argument, right to left, then evaluates
  the head. A λ consumes `Arg`, binds its name and enters the body. Builtins
  force to cached λ-chains ending in `CompKind::Builtin` (δ); β supplies
  their arguments, and δ reads the entry and arguments from its environment.
  A returned value meeting `Arg` is an invariant failure, never implicitly forced.
  Returned-thunk splits follow the [[invariants/ir-pure-cbpv|IR invariant]].
  Pushing n arguments needs n free frames.
- `force_val` enters a literal thunk in the current environment without
  closing it. `force` enters a stored thunk or a builtin's η-expansion over
  its captured entry; arity zero enters δ immediately. `call` runs a builtin
  directly at exactly its arity, otherwise pushes arguments and forces it.
  Under-application yields an ordinary λ residual. `dispatch_exec` handles
  boundaries with their checked site; inference-built partial boundaries
  remain λ-closures over `Exec`.
- Every `let` runs its RHS. `Bind` pushes `Frame::To`, which binds the result,
  boxing a residual function; `Seq` pushes `Frame::Seq`. These alone carry
  saved environments for their continuations. A lambda binds a name;
  parameter destructuring runs through body binds at parameter spans.
- `If` forces its selected arm; `step_case` first pushes one `Arg` holding
  the variant payload. Bare tags carry `Unit`. `Try` passes an error record
  through `call`, including its exact-arity builtin shortcut.
- `Guard` checks room for three frames and polls before shielded acquisition.
  `Frame::Acquire` restores scope, installs `Frame::Guard { release, resource }`
  and calls the body without an intervening poll. `guard_release` saves the
  outcome in `Frame::Cleanup`, shields release and calls it with resource
  plus `exit_case`. Release return restores the outcome; its halt overrides.
  Later guard reservations assert the room reserved at entry. Both shield
  frames restore scope on halt and abandon; a halting acquire runs no release.
  Shields change `scope`, not the worker cradle: workers follow their owner.
  A `gather` opened inside acquisition or release captures that shield
  ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).
- `Capture` swaps stdout for a buffer. Success restores the sink and returns
  exact `Bytes`; overflow refuses a prefix. A halt restores the sink and
  drops the buffer, adding its actual byte count to `Error.unwound.discarded`
  saturatingly on errors. Nested counts render as one note.
- `Rec` unfolds the selected member of a shared `GroupNode`. Every sibling
  closes over one environment restricted to the group's occurrence set;
  recursive references re-enter through force without rewriting bodies.
- Every computational frame is pushed through a `Slot` that `reserve`/`reserve_n` checked
  against `session.stack_limit` before any effect. `push_args` reserves all n
  `Arg` frames at once. Consuming `Arg` enters the body in tail position;
  no consumed call frame remains.
- `Frame::abandon` supplies undo to the rules and panic walk. Only `Seq`
  and a written `let` set the abandoned-tail fact; compiler-written binds
  do not set it merely for abandoning an operand or split call.
- `Frame::CallSite` restores a visible call's register on return, halt or
  panic. Adjacent tail-call restorations coalesce, and these provenance-only
  frames do not count toward the computational stack cap. Synchronous
  operations use `at_site`.
  See [[decisions/261105_the-error-record|the error record]].
- `Use(module)` calls `shell::load::run_module` on the already checked tree
  under Σ and returns its public bindings as a record. Each evaluation runs
  the body again; none resolves, reads or checks a module. Runtime failures
  retain the `use` context. Its operation name is `use`.
- Shell rules dispatch `Exec`, launch and join a `PipeNode` for `Pipeline`,
  and birth workers for `Spawn`. `Frame::Gather` owns a `BroodGuard`, captures
  the current scope in its new cradle and restores the displaced cradle
  before the shared brood-exit machinery
  joins or stops children. See [[internals/pipeline-execution|pipelines]]
  and [[invariants/a-worker-ends-with-its-children|worker scope]].

## Values and primitives

- `runtime/value.rs`: `ValClos::close(val, env, sig)` closes inert syntax.
  Variables use `lookup`; other forms restrict ρ to occurrences and pull Σ's
  answers for missing direct names into the capture, never descending a thunk.
  `View` reads one layer; components share ρ without re-restriction. Lists
  and maps expose `ListRef` / `MapRef`; a variant's resolved unit payload is
  absent in its view. See
  [[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].
- `expr.rs`: `eval_prim` interprets every `Prim` on values. `field`,
  `position` and `lookup` implement `Key::{Label, Position, Data}` reads;
  `position` is shared with `nth`. `Render` checks scalars; `Concat` joins
  strings. `Decode` drops its environment before taking over the bytes,
  removes one trailing terminator and checks UTF-8 strictly.
- `assemble.rs`: `eval_list` splices list values, `eval_update` replaces
  fields over one record base, and `eval_map` assembles map values. All
  operands have already run in surrounding binds. Explicit map entries
  beat spreads, first spread wins, and a repeated computed key fails here
  after every operand ran; known duplicates are refused statically.
- `pattern.rs`: `destructure` matches `Pattern::{Wildcard, Name, List,
  Record}`, collecting all bindings before installing any. A list without
  rest requires exact length. Mismatches are catchable errors; lambda
  parameter failures point at the parameter's span.

## Shell interpretation

- `Within` forms only its option record, then calls `WithinScope::parse(opts,
  shell)` in `scope.rs`. `handlers:` builds per-name entries from the map;
  `handler:` takes the checked value directly. Neither is re-vetted: the
  static check is total. Hoisted operand computations run in written order;
  the inert option fields are then formed in sorted key order (`dir`, `env`,
  `handler`, `handlers`). Naming either handler option installs a frame,
  even for an empty map.
  `WithinUndo` restores cwd, env overrides and handlers.
  Grants attenuate authority; guard frames live in `machine.rs` and release
  restores the body's outcome unless release itself halts. Handler dispatch is
  [[internals/handler-dispatch|deep and self-masking]].
- `call.rs`: `run_native` runs each native body inside `BuiltinEntry::framed`.
  The builtin may re-enter the machine; it is not a value-only primitive.
- Redirect installation and capture helpers live in
  [[map/core/runtime|shell/exec]] (`RedirectState`, `with_redirects`,
  `with_capture`). `STAGE_EVAL` supplies `evaluate` and `close_argv` to
  pipeline execution; a ral stage starts a fresh machine on its thread.
- Residual data is readmitted before execution, never by forcing a thunk or
  consuming a handle. The checker refuses a later unit's use of a stored name
  whose scheme delays a weak residual; no frame checks a returned value
  against a contract. A decided function beside undecided data needs only
  head admission, since its defining unit checked its body and registered
  boundaries return first-order data ([[map/core/typecheck|typecheck]]).
- `Tilde` reads live `HOME` through `Context::require_home`. Ambient reads
  are natives; `$SCRIPT` is a literal baked by elaboration.

The loop catches panics and abandons frames; the framed run restores its
checkpoint. Hot paths poll cancellation cooperatively. See
[[map/core/shell-state|shell state]], [[map/core/runtime|runtime]].
