---
generated_at_commit: 1e9fea4
generated_at_date: 2026-08-06
covers_paths: [core/src/typecheck/, core/src/typecheck.rs, core/src/mode.rs]
---

# Map: core / typecheck

`core/src/typecheck/` is Hindley–Milner inference over the CBPV
[[map/core/ir|IR]]. Types sit on `Val` and `Comp` after
[[map/core/elaboration|elaboration]].

Entry points (`typecheck.rs`):

- `typecheck(comp, SessionSchemes) -> Result<Comp, Vec<TypeError>>` — this
  function checks a program. On success it returns an *annotated* `Comp`.
  `annotate` rebuilds the IR after inference and writes three things onto
  it: a generalised `Scheme` on each top-level `Bind` node, resolved against
  the final unifier and closed by quantifying its residuals — that is,
  generalised against the empty environment; a ground `Wire` on each
  `Pipeline` stage; and a `Capture` node wherever a value demand meets a
  computation whose `result` mode grounds `Bytes`
  ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]],
  [[map/core/ir|ir]]). A `Pipeline` also carries `stage_types`, one resolved
  value type per stage, parallel to its stages and wires. `infer_pipeline`
  records each stage's return value alongside its spec in
  `InferCtx::stage_types`, keyed by stage address; `annotate` resolves each
  entry against the final unifier and writes the `Vec<Ty>` onto the node —
  the data that flows between stages, kept for the structural REPL's typed
  spine. This step retains what the pipeline check already computes; it
  adds no inference. The evaluator never reads `stage_types`, so an
  un-annotated stage keeps the elaborator's `Unit` placeholder without harm.
  The seed for a check is one `SessionSchemes { bindings, aliases,
  builtins }`
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]):
  the scope's name-to-`Option<Scheme>` map, the alias arms' schemes, and the
  shell's own `BuiltinTable`. Builtins are shell-scoped, so the checker
  types against exactly the surface the booted shell dispatches
  ([[map/core/builtins|builtins]]). `seed_env` is the one seeding routine.
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
  yields a *known* head's resolved `PipeSpec`, and `fresh_spec` for an unknown head
  — fresh input and output modes over a ground `∅` result
  — so reinterpreting a known head with incompatible modes is the lone rejected
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
  IR with schemes, ground wires, and `Capture` nodes;
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
(a value edge cannot meet a byte edge, `docs/SPEC.md` §4.2.1, §20.4) is one plain
method, `Unifier::unify_mode` (`core/src/typecheck/unify.rs`), reached through
the single variable store the single engine keeps
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
(`from-json : ⟨Bytes, ∅, ∅⟩ A`) is a static T0012. The `from-*` decoders are
arity-0 for the same reason — their bytes come from the channel, never from an
argument — so `from-json $x` is the static `DecoderTakesNoArgument` (T0054),
whose hint names the encoder-pipe remedy ([[design/codecs|codecs]]). A Step-shaped piped value (a
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

## The result mode

`PipeSpec` (`core/src/mode.rs:31`) carries three modes: `input`, `output`,
and `result`. `result` is a `PipeMode`, ground to `None` or `Bytes` at every
source-tree node. `result` names which conduit carries a computation's
payload: `Bytes` for the byte channel, `None` for the return value.
`result` rides the same unification, generalisation, and display code as
`input` and `output`.

Two well-formedness conditions guard every `Return` type the checker
builds. WF-1 states `result ⊑ output`: a byte payload needs a byte channel.
WF-2 states that `result = Bytes` implies a `Unit` return type: a byte
payload leaves no separate value. The checker asserts both where it
constructs a `Return` type: `Inferencer::seal` (`scope.rs:79`), `ret_bytes`
and `builtin_sig_result` (`builtins.rs:337`, `builtins.rs:1184`), and
`merge_branches`, `join_arm_results`, and `infer_pipeline` (`infer.rs:368`,
`infer.rs:427`, `infer.rs:1231`). `external_exec_comp_ty` (`infer.rs:849`)
satisfies WF-1 by inspection: an external command's `output` and `result`
are both the literal `Bytes`.

`Unifier::unify_mode` equates two `Return` types' `result` modes alongside
`input` and `output` (`unify.rs`); a ground `result` clash is an ordinary
mode-mismatch error, with no subsumption inside unification.
`Unifier::fresh_spec` mints a `PipeSpec` with `result: None`, the default
for a head whose modes are not yet known. `typecheck::builtins::sig_pipe_spec`
maps a command signature's own `result: ByteMode` field onto the built
`PipeSpec`'s `result`.

A `result` variable is never minted by a source typing rule. It appears
only in two declared signature slots: a builtin's computation-typed
argument (`ArgTemplate::BlockOrLambda`, for `spawn`, `each`, `map`, `fold`,
and their neighbours) and a scope's expected arm shape (`infer_try`,
`infer_guard`, and their neighbours in `scope.rs`). Both slots mint a bare
fresh `CompTy`; `Inferencer::extract_return` then mints the free `result`
mode, quantified in the surrounding scheme like any other mode variable.
No elaboration decision reads a slot variable.

`join_arm_results` (`infer.rs:386`) computes a branch join's `result`. A
byte-payload arm pins every free-result arm to `Bytes` and accepts each
`∅`-at-`Unit` arm by subsumption; a `∅`-result arm whose value type is not
`Unit` fails to unify, a genuine conduit mismatch rather than a coercion.
`infer_pipeline` (`infer.rs:1129`) grounds a byte-tailed final stage's own
`result` onto the whole pipeline: with no downstream consumer, such a
pipeline returns `Unit` and its `result` is `Bytes`, so a bound pipeline
value captures the last stage's bytes.

`CompKind::Capture(body)` types through `Inferencer::infer_comp`
(`infer.rs:1709`): its own `result` and `output` ground `None`, its `input`
is `body`'s, and its value type is `String`. This rule fires only when
re-inferring a tree that already carries `annotate`-inserted `Capture`
nodes — a stored handler or thunk re-checked at a later install.

`annotate.rs` inserts `Capture` during its write-back walk, as demand
propagation. A `Demand` is `Value` or `Discard`. It reaches a `Seq`'s tail,
a `Bind`'s `rhs`, each arm of an `If`, `Chain`, or `Try`, and the body of a
force of a syntactic thunk. Where a `Value` demand meets a node whose
recorded `result` grounds `Bytes`, `annotate_demand` wraps it in `Capture`.
`ArmWalk` (`annotate.rs:37-48`: `Plain`, `Descend`, `Wrap`)
decides how a join arm is rebuilt; `Wrap` is the subsumption instance,
wrapping a whole `∅`-at-`Unit` arm so its `Capture` contributes the empty
string. `annotate_join_arm` (`annotate.rs:272`) dispatches a `Comp` arm
this way. An opaque scope arm has no arm syntax to wrap, so
`eta_expand_captured` (`annotate.rs:336`) η-expands it instead.

`docs/SPEC.md` has the typing judgments.
