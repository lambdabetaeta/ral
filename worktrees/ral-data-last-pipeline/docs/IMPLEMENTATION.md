# ral — implementation notes

This document describes how the current implementation works.  It is
descriptive, not aspirational: behaviour here should match the code under
`core/src/` and `ral/src/`.  For the language itself see `SPEC.md`; for the
reasoning behind individual design decisions see `RATIONALE.md`.

## Workspace layout

The project is a two-crate Cargo workspace:

- `core/` — crate `ral-core`.  The language engine: lexer, parser,
  elaborator, typechecker, evaluator, pipeline runtime, builtins, sandbox.
  Also holds `prelude.ral`.
- `ral/` — crate `ral`.  The shell binary: CLI entry, REPL, Unix job
  control, and a build script (`ral/build.rs`) that bakes the prelude into
  the release artefact.

The split means `ral-core` can be linked into other hosts (embedded
evaluator, test harness) without pulling in REPL-specific dependencies such
as `rustyline`.  `ral/build.rs` parses, elaborates, and typechecks
`core/src/prelude.ral` at compile time, writes the IR and the per-binding
type schemes to `OUT_DIR/prelude_baked.bin` and `OUT_DIR/prelude_schemes.bin`
(postcard-serialised), and the resulting bytes are embedded into the binary
via `include_bytes!`.  The prelude is therefore never parsed at runtime.

## Top-level modules in `core/src/`

| File | Role |
| --- | --- |
| `lib.rs` | Public API surface (`parse`, `elaborate`, `bake_prelude_schemes`, …). |
| `lexer.rs` | Single-pass, modeless tokeniser. |
| `path.rs` | Shared path resolution/normalisation for scoped cwd semantics. |
| `parser.rs` | Surface syntax → `Ast` enum. |
| `elaborate.rs` | `Ast` → CBPV core (`Val`, `Comp`); SCC analysis for let-groups. |
| `classify.rs` | Shared head-classification table used by typechecker and runtime. |
| `ty.rs` | Pipeline-mode types (`Mode`, `HeadSig`, `ValType`, …) shared across passes. |
| `typecheck/` | Hindley–Milner inference with row types (see below). |
| `evaluator.rs` | CBPV interpreter. |
| `pipeline.rs` | Multi-stage pipeline orchestration. |
| `builtins.rs` | Central registry macro; dispatches to submodules. |
| `builtins/` | `concurrency.rs`, `collections.rs`, `control.rs`, `codecs.rs`, `modules.rs`, `util.rs`, `uutils.rs`. |
| `types.rs` | Runtime `Value`, `Env`, `EvalSignal`, `Error`. |
| `io.rs` | Stream abstraction for pipeline stages. |
| `signal.rs` | SIGINT/SIGTERM handling and child propagation. |
| `sandbox/` | Per-platform sandbox (`linux.rs`, `macos.rs`, `windows.rs`), plus `ipc.rs`, `spawn.rs`. |
| `diagnostic.rs` | Error rendering with source spans. |
| `compat.rs` | Windows-console VTP setup and `isatty` shims. |
| `debug.rs` | `debug_trace!` macro (compiled out in release). |
| `prelude.ral` | Prelude source (see above). |

`ral/src/` contains `main.rs` (CLI), `repl.rs` (readline-based interactive
shell), and `jobs.rs` (Unix job control, `fg`/`bg`/`jobs`).

## Pipeline: source → tokens → Ast → Comp → typed Comp → runtime

```
lexer → parser → elaborate → typecheck → evaluate
```

All modes — `ral script.ral`, `ral -c '…'`, and REPL — go through the same
pipeline.  The REPL re-enters it for every accepted line with the
accumulated top-level bindings kept in scope.

### CBPV core

The language is levelled in the style of call-by-push-value (Levy 2001):

- `Val` — inert value forms: `Unit`, `Literal`, `Variable`, `Thunk`, `List`,
  `Map`, `Tilde` (home-directory).
- `Comp` — computation forms that may have effects: `Force`, `Lam`, `Rec`,
  `Return`, `Bind`, `App`, `Exec`, `Pipeline`, `Background`, `Seq`,
  `LetRec`, control flow primitives, and a number of specialised nodes used
  by the runtime.

A thunk is a suspended computation packaged as a value; `Comp::Force`
re-enters it.  `Return` is the explicit value-to-computation lift.  There
is no statement-level auto-execution of arbitrary values: the statement
position only accepts computations.

### SCC-based let groups

`elaborate.rs::emit_assignment_group` (~lines 290–400) scans a run of
adjacent `let` statements, builds a dependency graph (edge `i → j` when
name `j` appears free in value `i`), computes strongly connected components
via Kosaraju's algorithm (`compute_sccs`, `elaborate.rs:909`), and emits:

- multi-member SCCs (or singletons with self-edges) where every member is
  a lambda → a single `Comp::LetRec` node;
- everything else → plain `let` bindings.

Kosaraju returns SCCs with source components first.  Because our edge
direction is "dependent → dependency", the emission loop walks
`sccs.iter().rev()` so that dependencies are installed in scope before the
statements that reference them.

Shadowing within a run (the same name rebound) would collapse into a
single dep-graph node, so the group is split at every shadow point and
each half elaborated recursively.  Non-lambda bindings are never placed in
a `LetRec`: the runtime would eagerly evaluate their bodies against
placeholder thunks.

## Typechecker (`core/src/typecheck/`)

A Hindley–Milner system tailored to CBPV, with Rémy (1989) row
polymorphism for record types and a small algebra for pipeline I/O modes.

- `ty.rs` — type syntax: `Ty` for values (`Unit`, `Bool`, `Int`, `Float`,
  `String`, `Bytes`, `List`, `Map`, `Record(row)`, `Thunk(CompTy)`,
  `Handle`, `Var`), `CompTy` for computations (`Return(PipeSpec, Ty)`,
  `Fun(Ty → CompTy)`, `Var`).
- `unify.rs` — union-find over metavariables; row unification uses label
  swapping so `{a: Int | r}` unifies with `{b: Bool | s}` by inserting the
  missing labels into fresh tails.
- `scheme.rs` / `generalize.rs` — type schemes, monomorphic and generalised;
  `generalize` closes over metavariables not reachable from the ambient
  environment.
- `env.rs` — typing environment layered to mirror lexical scopes.
- `infer.rs` — bidirectional/algorithm-W style walk of `Comp` and `Val`.
  Plain `let` binders generalise their RHS (`Comp::Bind` path).
  `Comp::LetRec` runs inference on the whole group against fresh
  metavariables, closes the fixed point by unification, then generalises
  each binding once the constraints are settled (`infer.rs:686–707`).
- `builtins.rs` — hand-written schemes for every builtin and prelude export.
- `fmt.rs` — human-readable printing with α-renaming for error messages.

Type errors do not abort inference: they are collected into `ctx.errors`
with source spans and reported together after the pass finishes.

## Evaluator (`core/src/evaluator.rs`)

A direct interpreter over `Comp`.  Runtime values live in
`core/src/types.rs::Value` — `Unit`, `Bool`, `Int`, `Float`, `String`,
`Bytes`, `List`, `Map`, `Thunk`, `Handle`.

A `Value::Thunk { param, body, captured, comp_type }` carries:

- `param: Option<Param>` — `None` for a nullary block, `Some` for a
  parameterised closure;
- `body: Arc<Comp>` — shared IR pointer;
- `captured: EnvSnapshot` — the scope stack at construction time;
- `comp_type` — the inferred `CompType`, used by the pipeline runtime to
  decide whether a stage runs in byte or value mode.

`EvalSignal` is the control enum returned by every evaluation step:

- `Error(Error { message, status, loc, hint, kind })` — the `kind` field
  (`ErrorKind::PatternMismatch` vs `ErrorKind::Other`) is what lets
  `_try-apply` intercept only parameter-destructure failures and let every
  other failure propagate.
- `Exit(i32)` — `exit N`.
- `TailCall { callee, args }` — trampolined by the top of the call loop so
  prelude helpers that ultimately defer to a builtin do not grow the Rust
  stack.

### Environment and closures

`Env` is a `Vec<Rc<HashMap<String, Value>>>` of scopes.  `Env::snapshot()`
clones the `Rc`s to produce an `EnvSnapshot` that a thunk can close over
cheaply.  Mutation on the top scope uses `Rc::make_mut`, so assignments
from one thread do not corrupt a snapshot another thread captured.

### Pattern matching and `_try-apply`

Destructuring failures raise `ErrorKind::PatternMismatch`.  The
`_try-apply` primitive destructures an argument against a thunk's
parameter pattern and reports success/failure without taking down the
surrounding computation; it catches only pattern mismatches and re-raises
any other error.

## Pipelines (`core/src/pipeline.rs`)

Stages are classified as external (byte-mode) or internal (Rust-visible
value stream).  Adjacent external stages share an OS pipe from `os_pipe`,
so the kernel handles byte flow without extra copies.  Internal stages run
each in their own thread and communicate through `mpsc` channels; adapters
at the boundary encode/decode bytes↔values.

Stage input/output types are inferred from the thunk's `comp_type` via
`stage_type_from_value`; a stage whose mode cannot be derived is flagged
with `unknown_stage_type_error` before the pipeline starts.

Failure semantics:

- a non-final stage exiting with `141` (SIGPIPE) is not a failure;
  downstream simply closed its end;
- any other non-zero stage exit fails the pipeline;
- the final stage's status is reflected in `_status` / `env.last_status`.

## Concurrency (`core/src/builtins/concurrency.rs`)

- `spawn` / `_fork` — launches a child in a new `std::thread`, handing it
  the current `EnvSnapshot` and an `mpsc::Receiver` for its eventual
  result.  Returns a `Handle`.
- `await` / `_await` — blocks on the receiver, caches the resolved value,
  and returns the cache on subsequent awaits.
- `race` / `_race` — returns the first completed handle; losers are
  logically cancelled at the handle layer but their threads are not killed
  at the OS level.
- `cancel` / `_cancel` — marks the handle cancelled; a later `await` fails.
- `disown` / `_disown` — detaches the handle; a later `await` fails with
  `<disowned>`.

The thread-based model sets the limits of the API.  In particular, once a
thread is computing there is no portable cross-thread interrupt, so
cancellation is cooperative.

## Modules (`core/src/builtins/modules.rs`)

- `source FILE` — evaluates `FILE` in the current scope, merging its
  bindings (including `_`-prefixed names).  Cycles are detected through
  `env.use_stack`; a depth guard limits runaway recursion.
- `use FILE` — evaluates in a child scope, returns a map of public
  bindings (every name not starting with `_`).  Caches by absolute path in
  `env.use_cache`: once loaded, a second `use` of the same path is a
  constant-time map lookup and sees the values from the first load.  The
  cache is not invalidatable from inside the process; restart to pick up
  source changes.  (This is a deliberate simplification that fell out of
  the SPEC §3.3 / CHANGELOG §4.1 pass: `use-reload` and `clear-use-cache`
  no longer exist.)

Path resolution is relative to `env.current_script` (the file containing
the `use`/`source` call), and `use` additionally searches `RAL_PATH`.
`env.current_script` is swapped during module evaluation and restored by
an RAII guard so nested loads and error paths both leave it consistent.

## Builtins and dispatch (`core/src/builtins.rs`)

A single `builtin_registry!` macro generates the `BuiltinName` enum and
the `builtin_by_name(&str) -> Option<BuiltinName>` table.  Head dispatch
is split across elaboration and evaluation:

1. Value-head resolution — a bare token bound in the lexical scope
   resolves to a `Force(Variable(…))` call.
2. Command-head resolution — otherwise, the name is looked up as (in
   order) an alias in interactive mode, a `with` override, a builtin, and
   finally an external command.

Each builtin submodule under `builtins/` holds the implementations for one
feature area (concurrency, collections, control, codecs, modules, utility,
uutils integrations).  The `BuiltinCompHint` enum attached to each builtin
tells the pipeline-mode inferencer what shape of stage it represents
(`Value`, `Bytes`, `Branches`, `LastThunk`, `DecodeToValue`,
`EncodeToBytes`, `Never`).

## Error handling

Every runtime error is an `EvalSignal::Error(Error)` carrying a
user-facing `message`, optional `status`, source `loc`, optional `hint`,
and the `ErrorKind` classifier.  `diagnostic.rs` renders them with spans.
`_try` returns a flat `{cmd, status, stderr, line, col}` record for the
failing computation and suppresses propagation; `_audit` returns the
entire `ExecNode` tree regardless of outcome; `guard` guarantees cleanup
runs while preserving the body's outcome.

## REPL and jobs (`ral/src/`)

`repl.rs` uses `rustyline` with `RalHelper` for completion.  Each line is
parsed, elaborated, typechecked (if enabled), and evaluated incrementally
against the accumulated top-level environment.  Runtime errors are
rendered in a compact, single-line form
(`format_repl_runtime_error_compact`).  `jobs.rs` (Unix only) implements
`fg`, `bg`, and `jobs` on top of process groups and the shell's foreground
pgroup.
