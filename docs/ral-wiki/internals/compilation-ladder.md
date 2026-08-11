---
verified_at_commit: 95449d4
verified_at_date: 2026-08-10
anchors: [compile, compile_and_typecheck, CompileOutcome, SessionSchemes, bake_prelude, bake_prelude_to_out_dir, BakedPrelude, postcard, annotate, PipeYield, stage_types, Capture, ArmWalk, eta_expand_captured]
---

# The compilation ladder: source to typed IR

Source text descends a fixed ladder, and each rung hands the next a different
artifact. `core/src/lib.rs` exposes the whole descent as two functions: `compile`
(parse → elaborate) and `compile_and_typecheck` (parse → elaborate → typecheck →
`CompileOutcome`).

- **Text → tokens.** The lexer reads characters into tokens with no
  context-dependent rules — there is one lexer, not the several a POSIX shell
  needs. ([[map/core/syntax|syntax]])
- **Tokens → flat surface AST.** The parser builds a single flat `Ast` enum.
  The flatness is deliberate ([[decisions/260530_ast-stays-flat|ast-stays-flat]]):
  head classification (`^name`, `./x`, `~/x`, bare) happens here, but no
  desugaring does.
- **Surface AST → CBPV IR.** The elaborator is the *one* phase that knows about
  surface sugar. It enforces the [[design/cbpv|value/command split]] by binding
  effectful sub-expressions to fresh temporaries (a *binds* accumulator folded
  into `Comp::Bind` chains), resolves command heads against lexical scope, and
  runs `group_stmts` first to find mutually recursive binding groups, which
  lower to `LetRec` / `Rec`. What it emits carries no parser syntax
  ([[invariants/ir-pure-cbpv|ir-pure-cbpv]]). ([[map/core/elaboration|elaboration]])
- **IR → typed IR.** Hindley–Milner inference annotates the `Val` / `Comp` tree
  ([[design/types|types]]). The checker is a transformation, `annotate`. It
  rebuilds the inferred tree once, carrying a demand at each position. A
  demand is a value read here, or a value discarded here. The rebuild returns
  an annotated tree carrying four verdicts.

  - Each top-level name-bind carries the generalised `Scheme` it inferred,
    closed against the empty environment so the scheme outlives the per-run
    unifier
    ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
  - Each `Pipeline` carries one `PipeYield`: the checker grounds the last
    stage's route, with every unification variable defaulted away, and writes
    down the *answer* — `Last` to report the helper's returned value, `Unit`
    because a byte payload never crosses the process boundary. The route itself
    does not survive; every interior edge is a byte pipe allocated from
    position, so there is nothing per-stage to write
    ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  - Each `Pipeline` also carries `stage_types`, one resolved value type per
    stage. Only the structural REPL's typed spine reads a stage type.
  - The rebuild wraps a node in a `Capture` node wherever a value demand meets
    a payload route that grounds `Bytes`
    ([[internals/output-capture-and-detachment|output-capture-and-detachment]]).

  Generalisation happens at each `Bind`, along the SCC structure the
  elaborator already found. A non-recursive group generalises at its own
  binding point. A mutually recursive group stays monomorphic until its fixed
  point.

  A value demand reaches:

  - a `Seq`'s tail;
  - a `Bind`'s RHS;
  - each arm of an `If`, a fallback chain, or a `try`;
  - the body of a force of a syntactic thunk.

  Every other position is a discard. A discarded value never wraps in
  `Capture`.

  A join needs one further rule for an arm that grounds `None` at type `Unit`,
  inside an otherwise byte-payload join. `ArmWalk::Wrap` (`annotate.rs`) wraps
  that whole arm in `Capture`. Its own payload then reads as the empty string.
  Its bytes still reach the outer stream as effect. The arm rebuilds at its
  own, ordinary discard demand inside the wrap.

  A scope's arm is a `Val`, not a `Comp`. It may be opaque. An opaque arm
  needing a value payload η-expands through `eta_expand_captured`
  (`annotate.rs`) into `{ |e| capture (force $h e) }`. The expansion is sound
  because a scope forces its arm exactly once and never returns it. The arm's
  own identity is therefore never observed, so nothing can compare, print, or
  send the wrapper elsewhere.

  Demand propagation stops at a leaf, at an opaque force, and at an opaque
  scope arm. `case` stays strict and uncoerced. Its arms are fields of a
  record, with no fixed arity for a demand to walk.

  With the second mode-inference engine retired
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]), this
  rung is the *only* source of the evaluator's modes and `Capture` nodes. The
  pass runs on every evaluated path. Neither is ever re-derived at runtime.
  A node inference never visited keeps the elaborator's placeholder — `Empty`
  for a wire, `Unit` for a stage type. The verdict rides inside the comp;
  `CompileOutcome` is unchanged in shape. ([[map/core/typecheck|typecheck]])

Each run's check is seeded from the live session — one `SessionSchemes`, the
scope's name→scheme map plus the alias arms' schemes — so a binding made in one
run enters the next run's check at its inferred scheme rather than a fresh
variable. The evaluator installs each top-level bind's scheme next to its value,
so the seed never drifts from the values it describes.

The prelude is baked once at build time as a schema-less `postcard` blob of this
same IR, so any field added to `Comp`, `Val`, or `Pattern` invalidates every
emitted blob — a hazard pinned with `cargo:rerun-if-changed` in *one* place,
`bake_prelude_to_out_dir` (`core/src/boot.rs`), since the only encode site and
the only decode site (`BakedPrelude`) live there together as the host-embedding
seam ([[decisions/260610_host-embedding-api|host-embedding-api]]). The bake runs
the checker: it parses, elaborates, and hands the comp to `bake_prelude`
(`core/src/typecheck.rs`), which serialises the *annotated* prelude and harvests
its bind schemes from the same pass, so the baked list and a run's installed
schemes come from one harvest. The two blobs — annotated IR and scheme list —
land in `OUT_DIR`; a host embeds them through the `baked_prelude!` macro into a
`BakedPrelude`, decoded lazily on first use. The typed IR is then handed to the
[[internals/evaluator-machine|evaluator]], which a host reaches only through the
synchronous framed run doors ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

See also [[design/cbpv|cbpv]], [[design/types|types]]; map hub
[[map/core|core]]. The formal account is `docs/SPEC.md`.
