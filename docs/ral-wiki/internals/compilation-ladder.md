---
verified_at_commit: 1baac6d
verified_at_date: 2026-06-22
anchors: [compile, compile_and_typecheck, CompileOutcome, SessionSchemes, bake_prelude, bake_prelude_to_out_dir, BakedPrelude, postcard, annotate, Wire, stage_types]
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
  ([[design/types|types]]), generalising at each `Bind` along the SCC structure
  the elaborator already found — non-recursive groups generalise at their binding
  point, recursive groups stay monomorphic until their fixed point. The checker
  is a *transformation* (`annotate`): it returns an annotated IR carrying three
  kinds of verdict. Each top-level name-bind takes the generalised `Scheme` it
  inferred, closed against the empty environment so the scheme outlives the
  per-turn unifier
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]);
  every `Pipeline` stage and `Bind` RHS — at any depth — takes its *ground mode
  wire*, the checker's resolved `F[input, output]` with the unification variable
  defaulted away, which the evaluator reads instead of re-inferring
  ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]); and each
  `Pipeline` carries `stage_types`, one resolved value type per stage parallel to
  its wires — the data flowing between stages, retained for the structural REPL's
  typed spine, never read by the evaluator. With the second mode-inference engine
  retired ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]),
  this rung is the *only* source of the evaluator's modes: the pass runs on every
  evaluated path, so the wires are never re-derived at runtime. A node inference
  never visited keeps the elaborator's placeholder — `Empty` for a wire, `Unit`
  for a stage type. The verdict rides inside the comp; `CompileOutcome` is
  unchanged in shape. ([[map/core/typecheck|typecheck]])

Each turn's check is seeded from the live session — one `SessionSchemes`, the
scope's name→scheme map plus the alias arms' schemes — so a binding made in one
turn enters the next turn's check at its inferred scheme rather than a fresh
variable. The evaluator installs each top-level bind's scheme next to its value,
so the seed never drifts from the values it describes.

The prelude is baked once at build time as a schema-less `postcard` blob of this
same IR, so any field added to `Comp`, `Val`, or `Pattern` invalidates every
emitted blob — a hazard pinned with `cargo:rerun-if-changed` in *one* place,
`bake_prelude_to_out_dir` (`core/src/driver.rs`), since the only encode site and
the only decode site (`BakedPrelude`) live there together as the host-embedding
seam ([[decisions/260610_host-embedding-api|host-embedding-api]]). The bake runs
the checker: it parses, elaborates, and hands the comp to `bake_prelude`
(`core/src/typecheck.rs`), which serialises the *annotated* prelude and harvests
its bind schemes from the same pass, so the baked list and a turn's installed
schemes come from one harvest. The two blobs — annotated IR and scheme list —
land in `OUT_DIR`; a host embeds them through the `baked_prelude!` macro into a
`BakedPrelude`, decoded lazily on first use. The typed IR is then handed to the
[[internals/evaluator-machine|evaluator]], which a host reaches only through the
synchronous framed turn doors ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

See also [[design/cbpv|cbpv]], [[design/types|types]]; map hub
[[map/core|core]]. The formal account is `docs/SPEC.md`.
