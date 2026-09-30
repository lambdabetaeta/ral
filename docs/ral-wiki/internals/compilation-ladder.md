---
verified_at_commit: 8d868e18
verified_at_date: 2026-09-30
anchors: [compile, compile_and_typecheck, CompileError, SessionSchemes, ReturnContract, contract::Table, bake_prelude, bake_prelude_to_out_dir, BakedPrelude, postcard, annotate, stage_types, Capture, captured_string, capture_sites, eta_expand_arrow]
---

# The compilation ladder: source to typed IR

Source text descends a fixed ladder, and each rung hands the next a different
artifact. `core/src/lib.rs` exposes the whole descent as two functions: `compile`
(parse → elaborate) and `compile_and_typecheck` (parse → elaborate → typecheck,
a `Result` whose `CompileError` is `Parse` or `Types`).

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
  lower to an n-ary `Rec` with a projection per member. A statement sequence
  `a; b` is itself a binder — `a to _. b` — so a block is a right-nested chain
  of `Bind`s and the top level is a list of `Phrase`s (`Define` / `Run`). What
  it emits carries no parser syntax
  ([[invariants/ir-pure-cbpv|ir-pure-cbpv]]). ([[map/core/elaboration|elaboration]])
- **IR → typed IR.** Hindley–Milner inference annotates the `Val` / `Comp`
  tree ([[design/types|types]]). The checker is a transformation, `annotate`: a
  plain structural rebuild of the inferred tree carrying four verdicts.

  - Each top-level name-bind carries the generalised `Scheme` it inferred,
    closed against the empty environment so the scheme outlives the per-run
    unifier
    ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
  - Each `Pipeline` carries `stage_types`, one resolved value type per
    stage. Only the structural REPL's typed spine reads a stage type. Every
    interior edge is a byte pipe allocated from position, so there is nothing
    else per-stage to write
    ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  - Each boundary call carries the `Site` inference recorded for it.
  - A command the checker recorded as captured is wrapped as
    `cap M to d. decode d` (`CompKind::Capture`, `CompKind::Decode`, built by
    `captured_string`)
    ([[internals/output-capture-and-detachment|output-capture-and-detachment]]).

  Generalisation happens at each `Bind`, along the SCC structure the
  elaborator already found. A non-recursive group generalises at its own
  binding point. A mutually recursive group stays monomorphic until its fixed
  point.

  An arrow-typed right-hand side is η-expanded into a thunked λ
  (`eta_expand_arrow`), so every function-typed thunk's body is a syntactic
  `Lam`.

  Which commands are recorded is decided *before* inference, from syntax alone:
  `capture_sites` walks the right-hand side of each `let` along the positions
  its result comes from, and the `Exec` rule of inference answers `F String`
  for a recorded node after typing the call `F Unit`
  ([[internals/type-inference|type-inference]],
  [[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]).
  The rebuild places a `Capture` by looking a node up in that record.

  This rung is the *only* source of `Capture` nodes, and it runs on every
  evaluated path: nothing reaches the evaluator that asks whether to capture,
  and a capture is never re-derived at runtime. A node inference never visited
  keeps the elaborator's placeholder — `Unit` for a stage type. The verdict
  rides inside the comp; `CompileError` is unchanged in shape.
  ([[map/core/typecheck|typecheck]])

A loading form may also hand this rung a `ReturnContract` — one of the
declared `typecheck::contract::Table`s — and once inference has finished the
checker ascribes that table's closed keyset to the toplevel's own returned
type (`contract::ascribe`), in a scratch copy of the unifier, so the table
never enters inference. The rc's eleven keys, a plugin manifest's four and a
capability profile's six are vetted with the same spans as the rest of the
file, and the inferred type is what is checked rather than the syntax that
built it, so a key misspelled inside a spread is caught with one written out.
`()` is the empty keyset; a map, a list, a scalar, or (but for a manifest) a
function is refused with the spelling to use. A program whose return is a
variable — `from-json` — or a manifest factory's `Thunk` is left to its
loader's runtime door, which dispatches off the same table.

Each run's check is seeded from the live session — one `SessionSchemes`, the
scope's name→scheme map plus the alias arms' schemes — so a binding made in one
run enters the next run's check at its inferred scheme rather than a fresh
variable. The evaluator installs each top-level bind's scheme next to its value,
so the seed never drifts from the values it describes.

The prelude is baked once at build time as a schema-less `postcard` blob of this
same IR, so any field added to `Comp`, `Val`, or `Pattern` invalidates every
emitted blob — a hazard closed by each host's build-dependency on `ral-core`,
which reruns `bake_prelude_to_out_dir` (`core/src/boot.rs`), the only encode site, and
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
