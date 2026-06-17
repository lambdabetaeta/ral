# Name resolution: where a capability lives

**A name in head position resolves through a fixed layering, and which layer a
capability belongs to is a design decision — driven by whether it returns a
*structured value* or performs an *effect*, and by who owns it.** No single
mechanism is "the builtins"; this page answers *where* a capability lands and
*why*, leaving *what a builtin is* to [[design/builtins|builtins]], the *how* of
registration to [[internals/builtins-registry|builtins-registry]], and the
formal catalog to `docs/SPEC.md` §16.

## The layers a head name can be

From most reserved to most peripheral:

- **Control operators** — `within`, `grant`, `try`, `guard`, `audit` (with the
  syntactic `if` / `case` / `?`). Grammar arms carried as dedicated IR nodes, not
  builtins, because each manipulates dynamic frames or audit ownership that no
  value can observe ([[design/control-operators|control-operators]]).
- **Core builtins** — Rust atoms in `CORE_BUILTINS`, each a six-facet
  [[internals/builtins-registry|registry]] entry binding names, type rule, fixed
  arity ([[invariants/fixed-arity|fixed-arity]]), doc, and body together. What
  makes a capability one of these, and the shape of the set, is
  [[design/builtins|builtins]].
- **Underscore primitives** — `_type`, `_ansi-ok`, and the host `_ed-*` / `_plugin`:
  internals the prelude wraps, never called directly by user scripts.
- **Prelude functions** — ordinary ral bindings in scope before user code,
  wrapping the layers below for convenience (`for` calls `each`, `lines` splits on
  `\n`). They curry and shadow like any binding (`docs/SPEC.md` §17).
- **Bundled coreutils** — `ls`, `cat`, `cp`, `mv`, `rm`, … : Rust, in-process, but
  *not* registry entries. The [[map/core/runtime|runtime command layer]]
  dispatches a bare invocation through `coreutils_invoke`, riding the same
  capability chokepoint and wait/signal/exit boundary as a system binary.
- **Host builtins** — registered *above* core by the embedder (the REPL's `_ed-*`,
  exarch's edit/search atoms); core never inspects them
  ([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).
- **External commands** — resolved on `PATH`; their captured stdout decodes as a
  `String` (one trailing newline stripped), never re-lexed.

Coreutils running in-process beside the structured primitives is what lets ral be
a [[invariants/single-binary|single binary]] with no sibling helpers.

## The principle: which layer a capability belongs to

Three rules decide the placement, and they are not interchangeable:

- **Effects go through coreutils, never structured primitives.** There is no
  `copy-file`, `make-dir`, or `remove-file`; `cp` / `mv` / `rm` / `mkdir` are
  canonical. An effect returns no structured value, so wrapping it in a builtin
  buys nothing but a second spelling — and a second spelling is a second thing to
  keep capability-checked.
- **Structured queries are Rust builtins — a syscall bridge, not text parsing.**
  `list-dir`, `file-info`, `is-file` / `is-dir` / `exists`, `glob`, `resolve-path`,
  `temp-dir` answer with records and lists, replacing a shell-out to `stat` / `ls` /
  `dirname` and a re-parse of its text. The bytes→text→structured round-trip never
  arises.
- **Conveniences are prelude.** If a capability is expressible as a function over
  values and thunks under ordinary Hindley–Milner typing, it is a prelude binding,
  not a builtin — the same derivability test that keeps `for` / `retry` out of the
  grammar ([[design/control-operators|control-operators]]). A builtin earns Rust
  only by needing host state, a type rule HM cannot derive, or a syscall.

Ambient scope is the fourth placement, already settled: it is a control operator,
because it scopes frames over a body's whole dynamic extent rather than computing
a value.

The one boundary this layering does *not* draw is between bytes and values: that
crossing is named by the `from-X` / `to-X` codecs, which are core builtins like
any other ([[design/codecs|codecs]]).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (the layering is the pure-fragment/effect boundary),
[[design/builtins|builtins]], [[design/codecs|codecs]],
[[design/pipelines|pipelines]], [[design/control-operators|control-operators]],
[[design/grant|grant]]; [[internals/builtins-registry|builtins-registry]],
[[map/core/builtins|map: builtins]].
Cite: RATIONALE §"External commands return strings",
§"Bundled coreutils are mandatory in exarch, optional in ral",
§"Syscall bridge, not text parsing", §"Control flow is library";
`docs/SPEC.md` §4.2, §16, §17, §21.
