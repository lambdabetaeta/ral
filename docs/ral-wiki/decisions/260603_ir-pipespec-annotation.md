---
status: active
landed: [d4aa75be, e3c23b4b, edf47cd9]
---

# The checker writes its mode verdict into the IR

**Landed** in three commits: `d4aa75be` adds the ground annotation slots
(`ByteMode` / `Wire` in `core/src/mode.rs`, `CompKind::Pipeline` a struct variant
`{ stages, wires: Option<Vec<Wire>> }`, `CompKind::Bind.rhs_output:
Option<ByteMode>`); `e3c23b4b` writes them (the checker records per-node verdicts
during inference, `annotate` grounds them into a rebuilt IR); `edf47cd9` reads
them at the two consumers, with the dual-run `debug_assert!` live. The deletion
inventory below was parked for ADR 4 and has since landed there
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]], `602218d3`):
no engine is removed *here* — this page only adds the slots and proves the dual-run
agrees.

ral keeps two mode-inference engines: the static Hindley–Milner checker
(`core/src/typecheck/`) and a runtime engine (`core/src/ty.rs`) that re-infers
each bind's modes at evaluation time. **The checker writes its resolved mode
verdict into the IR, and the evaluator reads modes off the node rather than
inferring them.** This makes the compilation ladder's *typed-IR* rung
([[internals/compilation-ladder|compilation-ladder]] names "IR → typed IR" but
today emits a `CompileOutcome` that drops the modes back out) carry the modes it
already claims to compute, and it deletes the runtime engine wholesale.

This is the third of four decisions whose end state is a single engine. The arc:
session-scheme-continuity
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]) →
handler/alias mode preservation
([[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]])
→ IR mode annotation (here) → the mode pass made unconditional, `--no-typecheck`
retired ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]). The
through-line is **net code removal**: this is the page where the second engine
dies, and it adds only the annotation slots that justify the deletion.

## What we decide

- **Annotation is elaboration into a rebuilt IR, not interior mutation.** IR
  nodes are `Arc`-shared and serde-derived (`Comp = Spanned<CompKind>`,
  `core/src/ir.rs`; `Arc<Comp>` thunk bodies serialised by
  `core/src/serial.rs`). An `OnceCell` slot would not serialise; a side table
  keyed by node identity would not travel with a thunk body across
  `SerialValue` or the baked prelude. So the checker becomes a *transformation*:
  it returns an IR with mode slots filled, leaving the unannotated IR untouched
  for the `--no-typecheck` paths that still exist until ADR 4. The prelude bake
  follows suit: `ral/build.rs` and `exarch/build.rs` serialise the *annotated*
  IR (parse → elaborate → annotate), not the bare elaborated comp —
  `bake_prelude` already runs the inferencer at bake time, so the comp
  blob and the schemes blob come out of one checked pass instead of one
  unchecked and one checked walk. The transformation is `annotate(comp, ctx,
  spine)` (`core/src/typecheck.rs`) — a full structural rebuild over both the
  `Comp` and `Val` categories, descending into thunk bodies and the values
  nested in lists, maps, and variants. The `spine` flag carries the scheme
  discipline of [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]:
  it covers exactly the top-level Seq parts and a `Bind`'s `rest`, so only a
  top-level name-bind takes a generalised scheme, while wires and the `Bind` RHS
  output mode are written wherever inference visited, at any depth (a `Pipeline`
  stage inside a lambda body carries its wires). The two slots differ in weight:
  a `Wire` is a `Copy` two-valued pair and rides unboxed in the node, where the
  `Bind` scheme slot is `Option<Box<Scheme>>` because a scheme is large (clippy
  `large_enum_variant`).

  The checker records its per-node verdicts during inference and the rebuild
  reads them back: `InferCtx.stage_specs`, keyed by each stage's `Comp` address,
  written at the end of `infer_pipeline` once every adjacency is unified;
  `InferCtx.bind_outputs`, keyed by the `Bind` node, written by the Bind rule for
  every pattern — a `Fun` RHS records `∅`, because evaluating a lambda builds a
  closure and emits no bytes, not the body spec a `Fun`-traversing projection
  would report. Grounding happens once, in `annotate`: resolve each recorded mode
  through the final unifier, then `Var → Empty` (`InferCtx::ground`).

- **Annotations are ground — and the type makes that a compile-time fact.** The
  slot does not hold a `PipeSpec`, whose `PipeMode::Var` arm would let a residual
  variable ride into the evaluator. It holds a new two-valued ground pair — call
  it `Wire { input: ByteMode, output: ByteMode }` with `enum ByteMode { Bytes,
  Empty }` — so "annotations are ground" is enforced by the type, not by an
  invariant comment ([[map/core/ir|ir]]). Polymorphism lives only in schemes on
  the checker side; the IR carries the *instantiated, ground* wire at each use
  site. Residual mode variables — the 260601 ADR's parked branch-union fresh
  vars, and uninstantiated polymorphic outputs — are grounded by **one defaulting
  rule applied at annotation time**: `Var → Empty`. This is exactly today's
  behaviour, in two places that must agree:
  - `resolve_pipeline` already defaults an unresolved stage mode to
    `PipeMode::None` (`analysis.rs`, the `if m.is_var()` loop), and
  - `InferCtx::output_mode` already returns `PipeMode::None` for a `Var`-resolved
    bind-RHS output (`core/src/ty.rs`).
  The annotation pass performs that defaulting once, at the unification frontier,
  rather than at each read site.

- **Minimum slots, maximum deletion.** A `Wire` is added only where a consumer
  reads one:
  - **Per-stage on `Pipeline`.** `analyze_stage` reads `StageSpec.comp_type` to
    fill `Boundary::from_mode`, `PipelineKind::from_specs`, and the last-stage
    `last_output` (`analysis.rs`, `route.rs`). `resolve_pipeline(stages, wires,
    shell)` now takes the node's wires: `specs_from_wires` reads each
    `StageSpec.comp_type` off the ground wire (`Wire::spec`) and feeds it to
    `analyze_stage`, which no longer infers its own signature. The runtime engine
    survives at this site only as the unchecked fallback (`specs_from_runtime`,
    the `--no-typecheck` path, byte-for-byte) and as the debug-only cross-check
    (`runtime_specs`) — its deletion is ADR 4's.
  - **The RHS output mode on `Bind`.** `eval_bind_rhs` reads exactly one bit —
    "is the RHS byte-output?" — to decide stdout capture (`evaluator/comp.rs`,
    SPEC §4.3). It branches on the `Bind` node's `rhs_output`: when `Some`, the
    ground `ByteMode` is the verdict; when `None` (unchecked IR) it falls back to
    `InferCtx::new().output_mode`. In a debug build the annotated branch also
    asserts the ground mode against that runtime call — the same dual-run guard.
  - **No body wire at thunk roots — dropped unconditionally.** ADR 2's
    install-time check has two halves, and ADR 2 landed as `ed043029` serving
    *both* with ADR 1's mechanism: `WithinScope::parse`
    (`core/src/evaluator/scope.rs`) calls `typecheck::alias_arm_scheme` (now
    head-aware, `Result<Scheme, ModeMismatch>`, `core/src/typecheck.rs`) seeded
    from `shell.session_schemes()`, exactly as `install_alias` does
    (`core/src/types/shell/scope.rs`), and the pin (`pin_arm_to_head`,
    `core/src/typecheck/infer.rs`) runs inside that engine pass; the catch-all
    `handler:` arm is exempt. **Both halves run ADR 1's install-time engine and
    read no IR node**, so the maintainer's choice to keep computed `within` opts
    legal and mode-check them at install (ADR 2, resolved) does *not* resurrect a
    thunk-root consumer — the computed-`within` arm is mode-checked by the same
    engine run, after which its scheme is discarded (`HandlerEntry.scheme` stays
    `None`) where the alias half stores it on the frame. The stronger reason is
    that a *ground* wire cannot serve this consumer at all, by this page's own
    grounding rule: the install check unifies the arm's `PipeSpec` against the
    head's, and that unification exploits polymorphism — a divergent arm carries
    an unconstrained output mode var that pins to `Bytes` and passes, where the
    `Var → Empty` defaulting would freeze the wire to `Empty` and the
    wire-vs-head comparison would over-reject every mode-polymorphic arm. The
    check needs the arm's polymorphic spec, which only a scheme carries —
    "annotations are ground" is the very reason a wire cannot do this job.
  No other node is annotated. **The minimum slot set is two** — the per-stage
  `Pipeline` wire and the `Bind` RHS output mode — with no thunk-root slot and no
  conditionality. Every slot is justified by the consumer it lets read the node
  instead of re-walking it.

## The deletion inventory

This is the page's reason to exist — though the deletion itself lands in ADR 4
(`602218d3`), not here. With the IR carrying ground wires, the whole runtime engine
`core/src/ty.rs` is dead:

- The structural judgment — `constrain_comp`, `first_byte_input`,
  `seq_byte_output`, `infer_chain`, `infer_branches`, `infer_app`, `infer_force`,
  `constrain_val_thunk`, `command_sig`, `reducer_pipe`, `thunk_body_type`,
  `instantiate_from_args`, `named_head` / `last_effective` — and the
  `env_binding_type` walk (already shorn of its `lookup_handler` precondition by
  ADR 2). Each had a static twin in `typecheck/infer.rs`
  (`comp_output_mode` / `comp_input_mode` / `lift_seq_output` / the pipeline
  stage loop); the twin survives, the runtime copy goes.
- The `ModeUnifier` union-find (`core/src/ty.rs`) and `InferCtx` itself.
- **The prelude byte-mode residue** `classify::prelude_command_spec` /
  `PRELUDE_BYTE_FNS` (`core/src/classify.rs`) — a *third* copy of mode knowledge,
  the hand-maintained list `["map-lines", "filter-lines", "each-line", "view"]`
  that the shell-free fallback consults because it cannot walk a prelude body.
  The baked prelude's schemes already carry the real inferred modes for these,
  and the IR annotation supersedes the residue. `core/src/classify.rs` is deleted
  in full ([[map/core/syntax|syntax]]).
- **`validate_pipeline`'s runtime unification and the `pipeline_mismatch`
  diagnostic** (`analysis.rs`). A `∅`-into-`Bytes` adjacency is rejected by the
  static checker (260601 made `pipeline_mismatch` reachable statically, T0012);
  on annotated IR the pipeline's wires are already ground and consistent, so the
  runtime unification is unreachable. It is demoted to a `debug_assert!` that the
  adjacent wires agree, or deleted outright once ADR 4 makes the check
  unconditional.
- **The `ModeStore` trait and the genericity of `unify_mode`**
  (`core/src/mode.rs`). The trait existed for exactly one reason: to drive the
  equality rule through *both* engines' variable stores so the two could not
  drift (260601's "neither can drift again"). With one engine left, the sharing
  mechanism has done its job and retires — `unify_mode` folds into the checker's
  unifier as a plain method, and `ModeStore` / the runtime `ModeSlot` /
  `ModeVar` union-find go with it. The lattice itself (`PipeMode`, `PipeSpec`)
  stays: it is what schemes serialise and what the annotation pass grounds into a
  `Wire`.

What **survives** is honest about itself. The brief asks whether
`CommandTypeResolution.internal` / `command_is_internal` is classification rather
than inference. A source read says: the `internal` field is dead — it is
*written* at three sites in `core/src/ty.rs` and *read nowhere*. The genuine head
classification that pipeline staging consumes is the separate
`command_call::resolve_command_word → Resolution` and `classify_stage`
(`analysis.rs`), which decide Ral vs External vs Uutils dispatch and do not touch
modes. So `CommandTypeResolution` and `command_is_internal` are deleted with the
rest of the engine — not because they are inference, but because the only thing
they classified into is unused. The surviving classifier already lives where the
launcher needs it ([[internals/pipeline-execution|pipeline-execution]]). Nothing
else in `ty.rs` is classification.

## Transition and dependencies

- **Dual-run, then delete.** The annotation pass landed first (`e3c23b4b`); the
  consumers then switched to reading the `Wire` off the node (`edf47cd9`), with a
  `debug_assert!` comparing each annotation against the old runtime engine's
  verdict over the test suite — `runtime_specs` for the pipeline stages (no argv
  re-evaluated), `rhs_is_byte_output`'s assert for the bind RHS. The dual-run is
  **green across the full suite** — 33 binaries, 0 failures, debug build with the
  asserts live; clippy clean; the `-D warnings` build holds. **The deletions in
  the inventory above land only when ADR 4 lands**, because the fallback for
  *unchecked* IR (`--no-typecheck`, `ral/src/main.rs`) is the old engine, and ADR
  4 is what removes that fallback. This page's job is to make the annotation real
  and prove it agrees; ADR 4 collects the corpses.
- **What the dual-run caught.** The page predicted "exactly the kind of
  disagreement the dual-run `debug_assert!` exists to catch." A seventh, the
  earliest, predates the dual-run — found while landing ADR 1 (commit `03a3fee`):
  `consumes_value_arg` (`core/src/typecheck/infer.rs`) misread a
  ground-`Bytes`-input stage whose return type was polymorphic
  (`from-json : F[Bytes,∅] α`) as a value-arg consumer; the static twin was
  aligned to make that edge a static T0012. The dual-run itself surfaced **six
  more**, each fixed *at the root* — on whichever engine was wrong — rather than
  by weakening the assert:
  1. **An external head's input is open.** The static `external_exec_comp_ty`
     always said `F[var, Bytes]`; the runtime `ext()` default disagreed with
     `F[Bytes, Bytes]`. The runtime was aligned (new `external_spec` in
     `core/src/ty.rs`, and `command_sig`'s unknown-external branch). Consequence:
     `'abc' | blah` is well-typed through the open input and fails at
     `blah`-not-found, not as a channel mismatch.
  2. **`named_head` peers through `Force(Thunk(..))` and a `Bind`'s `rest`.** So
     `!{ f x }` and a hoisted-interpolation bind resolve their head through scope
     instead of defaulting a local name to external bytes (`core/src/ty.rs`).
  3. **`try`/`guard`/`audit` own no byte output.** They return a value, so their
     own output channel is `∅` while the body's bytes flush through — matching the
     static `infer_try`/`infer_guard`/`infer_audit`, each `CompTy::pure(α)`
     (`core/src/typecheck/scope.rs`). The alignment exposed **wrong goldens**:
     `tests/builtins/audit.out` and `audit-tree.out` were missing body lines that
     the old bind-capture swallowed — bytes captured for a non-`Unit` result,
     then discarded, printed nowhere and bound nowhere. The goldens were re-blessed
     with the complete output; this is a user-visible bug the alignment fixed. The
     check exists to catch engine disagreement, but here it caught the *tests*
     encoding the buggy engine's behaviour.
  4. **`env_binding_type` resolves lexically and breaks recursion.** It resolves a
     closure body's free names against its captured environment (lexical, as the
     checker does), treats a bound non-callable value as `∅` rather than external
     bytes, and breaks recursion with a `seen` set so a self-referential binding is
     mode-neutral (`core/src/ty.rs`, `env_binding_type_seen`).
  5. **A value-arg-consumed stage reports its own channels.** `infer_pipeline`
     records such a stage on `F[∅,∅]` (the `consumed_as_value` flag,
     `stage_own_spec`) rather than its application body's — matching how staging
     launches a `{ |x| … }` block stage (`core/src/typecheck/infer.rs`).
  6. **The pipeline's tail output reads the same own-channel spec.** When the last
     stage is value-arg-consumed, `infer_pipeline` reads the pipeline's output
     mode off `stage_own_spec` of that stage (`F[∅,∅]`) rather than its applied
     body's, so the wire on the final stage matches the runtime's reading of the
     block-stage launch.
- **Depends on ADR 1.** The checker carries each session binding's scheme
  forward, so a session-bound call site is annotated with a *real* wire, not a
  fresh-variable-defaulted-to-`Empty` one.
- **Depends on ADR 2.** No handler or alias may change a head's modes after the
  check, so an annotation written at compile time stays valid through any dynamic
  rebinding — the precondition for a verdict that travels with the node.

**Verify:** the dual-run `debug_assert!` holds across the full test suite,
including the pipeline tests, with no annotation diverging from the old engine's
verdict; a representative pipeline (`printf "a\nb\n" | map-lines { |x| echo $x }
| from-lines`) wires its stages from the annotated `Wire`s, and the
`--no-typecheck` fallback runs byte-for-byte through the old engine (compiled out
only in ADR 4); a `Lambda` whose body carries interior wires (on its
`Pipeline` stages and `Bind` RHSes — there is no thunk-root wire) round-trips
through `SerialValue` and a sandbox re-exec with those annotations intact
(`core/src/serial.rs`, `core/src/sandbox`); and the prelude re-bake picks up the
`CompKind`/`Wire` schema change automatically via the `cargo:rerun-if-changed`
lines the build-dep hazard note mandates (`core/src/lib.rs`, `ral/build.rs`,
`exarch/build.rs`) — a stale blob must not deserialise into the new shape.

See also
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]],
[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]],
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
[[internals/compilation-ladder|compilation-ladder]],
[[internals/pipeline-execution|pipeline-execution]],
[[map/core/ir|ir]], [[map/core/evaluator|evaluator]].
