---
generated_at_commit: 64b60eafa2eec1a958ff29d8671376d79856dc3a
verified_at_commit: b1ba0713
verified_at_date: 2026-11-03
anchors: [compile, compile_and_typecheck, CompileError, SessionSchemes, ReturnContract, contract::Table, bake_prelude, BakedPrelude, postcard, Readback, Inferring, Mode, Capture, captured_string, Closed]
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
loader's runtime check, which dispatches off the same table.

After inference, nested checks and return-contract ascription succeed,
`Readback::toplevel` maps the pending tree to the checked `Toplevel`. One
`UnitId` identifies all site snapshots and the first-use readmissions in
`Toplevel.admits`. Imported value membranes survive only when their solved
contract has delayed parts; existing checked sites are kept unchanged.
Inference and readback each memoise recursive groups by `Arc` identity, so
projections share one group through both phases. An erroneous unit is refused
whole and its pending IR discarded; one-shot inference sessions also discard
their pending IR.

Each run's check is seeded from the live session — one `SessionSchemes`, the
scope's name→scheme map plus the alias arms' schemes — so a binding made in one
run enters the next run's check at its inferred scheme rather than a fresh
variable. The evaluator installs each top-level bind's scheme next to its value,
so the seed never drifts from the values it describes.

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
