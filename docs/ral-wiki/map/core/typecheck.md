---
generated_at_commit: f7cf93a
generated_at_date: 2026-07-25
covers_paths: [core/src/typecheck/, core/src/typecheck.rs, core/src/mode.rs]
---

# Map: core / typecheck

`core/src/typecheck/` is Hindley–Milner inference over the CBPV
[[map/core/ir|IR]]. Types sit on `Val` and `Comp` after
[[map/core/elaboration|elaboration]].

Entry points (`typecheck.rs`):

- `typecheck(comp, SessionSchemes) -> Result<Comp, Vec<TypeError>>` — check a
  program, returning an *annotated* comp on success: `annotate` rebuilds the IR,
  writing each top-level bind's generalised `Scheme` onto its `Bind` node (resolved
  against the final unifier, closed by quantifying residuals — generalisation
  against the empty environment) and the ground mode `Wire` onto every `Pipeline`
  stage and `Bind` RHS at any depth
  ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]). A
  `Pipeline` also carries `stage_types`, one resolved value type per stage
  (parallel to its stages and wires): `infer_pipeline` records each stage's
  return value alongside its spec in `InferCtx::stage_types`, keyed by stage
  address, and `annotate` resolves each against the final unifier and writes the
  `Vec<Ty>` onto the node — the data flowing between stages, kept for the
  structural REPL's typed spine. No extra inference, only retention of what the
  pipeline check already computes; the evaluator never reads it, so an
  un-annotated stage keeps the elaborator's `Unit` placeholder harmlessly. The seed
  is one `SessionSchemes { bindings, aliases, builtins }`
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]):
  the scope's name→`Option<Scheme>` map, the alias arms' schemes, and the
  shell's own `BuiltinTable` — builtins are shell-scoped, so the checker types
  against exactly the surface the booted shell dispatches
  ([[map/core/builtins|builtins]]); `seed_env` is the one seeding routine.
- `bake_prelude(comp) -> (Comp, Vec<(String, Scheme)>)` — called by the consumer
  `build.rs`: returns the annotated prelude comp alongside the schemes harvested
  off its `Bind` nodes (`harvest_schemes`), one walk behind both the build-time
  bake and a run's installs.
- `alias_arm_scheme(head, param, body, SessionSchemes) -> Result<Scheme, ModeMismatch>`
  — infers an alias arm under the runtime handler calling convention, pins its
  `PipeSpec` to `head`'s spec (`Inferencer::pin_arm_to_head`), and closes it, for
  `install_alias` and `WithinScope::parse` to store on a frame. A handler or
  alias arm is a *fixed-arity lambda* — its calling convention is the surface
  form, not the runtime value's shape, so `param` is non-optional and
  `infer_alias_arm` types the arm `Fun(List(elem), body)`, forcing it on the
  argv list ([[invariants/fixed-arity|fixed-arity]],
  [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  Statically `infer_handler_comp` still types a non-`Lam` thunk (e.g. a computed
  `alias g $h`) by its bare body, binding it so `g x` is an arity mismatch
  rather than a silently discarded argument; the runtime install boundary is the
  sole complete gate on shape. `head_pipe_spec`
  yields a *known* head's resolved `PipeSpec` and a fresh `F[μ, ν]` for an unknown
  head, so reinterpreting a known head with incompatible modes is the lone rejected
  failure while a fresh alias defines its own modes
  ([[decisions/260606_alias-head-defines-its-modes|a fresh head defines its own modes]]).

The sorts split with CBPV:

- value types `Ty` describe data;
- computation types `CompTy` describe effectful computations carrying pipeline
  modes (`PipeMode` / `PipeSpec` / `ModeVar`);
- records are open-row-polymorphic ([[design/row-types|row-types]]: `Row` /
  `RowVar`).

Generalisation happens at `Bind`; recursive bindings (`LetRec` / `Rec`) stay
monomorphic to keep generalisation sound.

Internals:

- `infer.rs` — the `Inferencer`; `infer_comp`;
- `unify.rs` — `Unifier`;
- `ty.rs` — the data-only type definitions (`Ty`, `CompTy`, rows);
- `scheme.rs` — `Scheme`;
- `error.rs` — the error taxonomy: `TypeError` / `TypeErrorKind`, with
  constraint provenance as data (`Reason`, `CompDiff`);
- `explain.rs` — the single home of every user-facing type-checker sentence
  (hints and `TypeErrorKind::render_label`), a pure function of the error data
  so each message is unit-testable;
- `annotate.rs` — the write-back pass (`annotate`) that rebuilds the checked
  IR with schemes and ground wires;
- `generalize.rs`;
- `env.rs` — `TyEnv`, `InferCtx`;
- `fmt.rs` — type display;
- `builtins.rs` — per-builtin type rules (`builtin_arity`, `builtin_type_hint`),
  whose arity rules enforce [[invariants/fixed-arity|fixed-arity]];
- `scope.rs` — the five structural scope nodes.

`infer.rs`'s `infer_case` is left as one ~100-line function by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]).

## One mode-inference engine, one lattice, one mode-unify rule

The static checker is now the **sole** mode-inference engine: the runtime engine
is deleted and the inference pass runs on every evaluated path, writing the
evaluator's mode wires
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).

The pipeline-mode lattice — `PipeMode` / `ModeVar` / `PipeSpec` and the
constructors `none`/`decode`, plus the ground `ByteMode` / `Wire`
the annotation pass grounds into — lives in `core/src/mode.rs`. The equality rule
(a value edge cannot meet a byte edge, `docs/SPEC.md` §4.2.1, §20.4) is now a plain
method `Unifier::unify_mode` (`core/src/typecheck/unify.rs`); the `ModeStore` trait
that once drove it through two variable stores is gone, its sharing job complete
with one engine left
([[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]).

A builtin's boundary modes are the modal projection of its declared signature,
read once. `typecheck::builtins::sig_pipe_spec` maps a command signature's result
template onto a `PipeSpec`; the checker builds its `CompTy` from it, minting open
modes with `Unifier::fresh_mode`. The streaming reducer `fold-lines` is the one
shape no structural projection can read, so its scheme factory bakes the boundary
directly via `typecheck::builtins::reducer_spec` (bytes in, output following the
callback) instead of reading it off a signature template; it registers as an
ordinary `BuiltinTypeRule::Scheme`.

The byte-output mode of the streaming reducers `map-lines`/`filter-lines`/`each-line`
(prelude wrappers over `fold-lines`) follows from the body: `fold-lines` is
mode-polymorphic in its callback's output, and a `Seq`'s byte-output is a join over
its statements (`infer.rs::lift_seq_output`) — bytes if *any* statement emits bytes.
A value edge is unconditionally data-last application — `x | f = f !{x}` — with
no structural recognition of streams ([[decisions/260609_pure-pipe-equation|pure-pipe-equation]]):
`consumes_value_arg` is the discriminator. When it holds, the value producer
feeds a value-arg function consumer and `infer.rs::apply_piped_value` flows the
produced value into the function's first parameter (one thunk deref via
`deref_forced_producer`, mirroring the single runtime force). When it does not,
the stage is a plain channel consumer and the modes unify directly, so a
`∅`-into-`Bytes` adjacency is rejected as the §4.2.1 mismatch it is.
`consumes_value_arg` resolves the stage's `spec.input` first: a stage whose
input is ground `PipeMode::Bytes` is a channel consumer regardless of how
polymorphic its return value is, so a `∅`-output producer feeding a byte decoder
(`from-json : F[Bytes,∅] α`) is a static T0012. A Step-shaped piped value (a
variant carrying `` `more `` / `` `done ``) is ordinary recursive data the
consumer receives whole; on a clash, `apply_piped_value`'s hint points at the
explicit `stream-each` / `stream-map` / `stream-to-list` eliminators —
diagnostic-only, never shaping the types
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

The `if`/`case` branch-mode *union* (`infer.rs::union_mode`/`merge_branches`)
widens a `Bytes`-vs-`∅` branch clash to a fresh variable rather than rejecting it,
since only one branch runs. Whether to keep this leniency or go equality-strict is
**open**, pending maintainer review (same decision page). A `?` fallback chain
(`infer.rs::infer_chain`) unions its arms' input and output modes the same way —
only one arm wins — while leaving the chain's value type a fresh variable, so a
chain of byte-output arms reads as byte-output
([[decisions/260606_alias-head-defines-its-modes|a fresh head defines its own modes]]).

`docs/SPEC.md` has the typing judgments.
