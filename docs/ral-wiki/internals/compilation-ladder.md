---
verified_at_commit: 2d05ef216180fc2454ccef6f4ed4d4c65bf69136
verified_at_date: 2026-11-11
anchors: [Solver, Settled, Solution, compile, compile_and_typecheck, CompileError, SessionSchemes, ReturnContract, contract::Table, bake_prelude, BakedPrelude, postcard, Readback, Inferring, Mode, Capture, captured_string, Closed, run_phrases]
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
  The flatness is deliberate:
  head classification (`^name`, `./x`, `~/x`, bare) happens here, but no
  desugaring does.
- **Surface AST → CBPV IR.** The elaborator is the *one* phase that knows about
  surface sugar. It enforces the [[design/cbpv|value/command split]] by binding
  effectful sub-expressions to fresh temporaries (a *binds* accumulator folded
  into `Comp::Bind` chains), resolves command heads against lexical scope, and
  splits each stretch of block definitions (SPEC §5.7) into its mutually
  recursive groups along the references that resolution itself finds; each
  lowers to an n-ary `Rec` with a projection per member. A statement sequence
  `a; b` is `Seq(a, b)`; a written `let`, including `let _`, is `Bind`.
  A block nests those continuations and the top level is a list of `Phrase`s
  (`Define` / `Run`). Lambdas bind names; destructuring becomes binds after
  the whole lambda chain, located at the parameters. Collection operands
  hoist in written order before their assembly primitive; interpolation
  hoists all parts, then renders and concatenates. A value head hoists before
  its arguments; a function-typed head is bound unrun and runs when applied.
  Elaboration refuses a spread into a written value head or lexically bound
  head; checking refuses one into a builtin or session binding.
  What it emits carries no parser syntax
  ([[invariants/ir-pure-cbpv|ir-pure-cbpv]]). ([[map/core/elaboration|elaboration]])
- **IR → checked IR.** Each Hindley–Milner inference rule returns its type
  beside the node it builds ([[design/types|types]]), in the `Inferring`
  phase: bindings and calls carry holes, boundaries their types. Boundary
  η-expansion is built at its rule; binding capture, Let-fun and
  returned-thunk splits are decided by suspended matches and applied by
  readback once the unit succeeds
  ([[internals/type-inference|type-inference]]).

  - Each `Phrase::Define` carries its harvested per-name `Scheme`s unsettled.
    The successful unit's readback settles weak references and residual grades
    with `settle_weak` into `Closed`, so the schemes outlive the per-run unifier
    ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
  - A `Pipeline` carries nothing per stage: every interior edge is a byte pipe
    allocated from position, so there is nothing to write
    ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  - Boundary calls carry their solved type; readback snapshots it as a
    `Site`. These graphs can retain weak variables and delayed obligations,
    not just ground types.
  - A binding whose mode is `Mode::Capture` is read back as
    `cap M to d. decode d` (`CompKind::Capture`, `Prim::Decode`, built by
    `captured_string`)
    ([[internals/output-capture-and-detachment|output-capture-and-detachment]]).
  - An application through a returned thunk is split into explicit
    `M to h. force h · V` segments, once per recorded split
    ([[invariants/ir-pure-cbpv|the IR invariant]]).

  Generalisation happens at each `Bind`, not `Seq`, along the SCC structure the
  elaborator already found. A non-recursive group generalises at its own
  binding point. A mutually recursive group is checked once, monomorphic
  within itself, and generalised as its region closes.

  An arrow-typed right-hand side is bound unrun, as its thunk:
  `Mode::Thunk` reads back as `return (thunk M)` (Let-fun, SPEC §17.5).

  Only a binding inserts a capture: its match reads the RHS's head, and
  readback applies `Mode::Capture` to the checked RHS; arguments and joins
  only unify
  ([[internals/type-inference|type-inference]],
  [[design/capture|grades and capture]]).

  This rung is the only source of inserted `Capture` nodes and runs on every
  evaluated path. The evaluator executes the explicit capture/decode terms;
  it does not decide where capture belongs. ([[map/core/typecheck|typecheck]])

A loading form may also hand this rung a `ReturnContract` — one of the
declared `typecheck::contract::Table`s. The phase order is **generate on
`Solver` → settle all remaining elaboration into `Settled` → hold the return
contract live → close gates, nested tables and errors into `Solution` →
freeze by readback**. If settling reports errors, the hold is skipped.
The contract can solve weak types but cannot select a `let` mode or call
split, or open a hole. Its field and factory equations reach decoder sites
before they freeze; the declared table is never unified as a record type.

The rc's thirteen keys, a plugin manifest's four and a capability profile's
six are judged over inferred types, including fields introduced by spreads.
The live walk re-reads row tails after each field; final keysets and unseen
fields are judged afterwards, the latter on scratch without repeated
inference. Nested tables remain scratch judgements at close, after live
holds. See [[internals/type-inference|type inference]] for this boundary.
`()` is the empty keyset; a map, list, scalar, or (but for a manifest) a
function is refused with the spelling to use. A manifest factory's shape and
result are held live too. Wholly variable returns and unresolved fields or
members retain their loader's runtime checks against the same table.

After settling, live return-contract holds and closure succeed,
`Readback::toplevel` maps the pending tree to the checked `Toplevel`. One
`UnitId` identifies all site snapshots and the first-use readmissions in
`Toplevel.admits`. Imported value membranes survive only when their solved
contract has delayed parts; existing checked sites are kept unchanged.
Inference and readback each memoise recursive groups by `Arc` identity, so
projections share one group through both phases. An erroneous unit is refused
whole and its pending IR discarded; one-shot inference sessions also discard
their pending IR.

Before any phrase runs, `run_phrases` readmits the whole checked `Toplevel`
against the `Env` supplied for its execution, using the session's fixings.
This holds in every mode: a loaded file is a unit, whether entered by `use`
or `load::evaluate_source`. A refusal returns the supplied environment
unchanged and no defined names; no phrase runs. It names the use that imposed
the type, with the loaded file's own location and, for `use`, its loader context.
See [[map/core/evaluator|evaluator]].

Each run's check is seeded from the live session — one `SessionSchemes`, the
scope's name→scheme map, handled names for diagnostics and the host's manifest.
A binding made in one run enters the next run's check at its inferred scheme
rather than a fresh variable. The evaluator installs each top-level bind's
scheme next to its value. Handler entries carry no schemes: head typing and
argument gates depend only on Σ and lexical bindings, not the dynamic stack.

The prelude is baked once at build time as a schema-less `postcard` blob of this
same IR, so any field added to `Comp`, `Val`, or `Pattern` invalidates every
emitted blob — a hazard closed by each host's build-dependency on `ral-core`,
which reruns the host's build script, whose one call to `shell::boot::bake_prelude`
(`core/src/shell/boot.rs`) is the only encode site; the only decode site
(`BakedPrelude`) lives there too, together as the host-embedding
seam ([[decisions/260610_host-embedding-api|host-embedding-api]]). The bake runs
the checker: it parses, elaborates, and hands the comp to `bake_prelude`
(`core/src/typecheck.rs`), which checks it against core's manifest and returns
the *annotated* prelude; each `Define` already carries its schemes, and
`SessionSchemes::from_prelude` reads them back, so there is no second list. The
one blob — annotated IR — is returned as bytes, which the host's build script
writes to `OUT_DIR`; a host embeds it through the
`baked_prelude!` macro into a `BakedPrelude`, decoded lazily on first use. The typed IR is then handed to the
[[internals/evaluator-machine|evaluator]], which a host reaches only through the
synchronous framed run entries ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

See also [[design/cbpv|cbpv]], [[design/types|types]]; map hub
[[map/core|core]]. The formal account is `docs/SPEC.md`.
