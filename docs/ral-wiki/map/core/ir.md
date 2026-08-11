---
generated_at_commit: f5720be
generated_at_date: 2026-08-11
covers_paths: [core/src/ir.rs]
---

# Map: core / IR

`core/src/ir.rs` is the [[design/cbpv|call-by-push-value]] intermediate
representation — the target of [[map/core/elaboration|elaboration]] and the input
to the [[map/core/evaluator|evaluator]].

The two categories:

- `Val` — inert data: `Unit`, `String`, `Int`, `Float`, `Bool`, lists, maps,
  thunks, variables. A value can never diverge or perform I/O.
- `Comp` — effectful, sequenced computation. `Comp` wraps a `CompKind` plus an
  optional `Span` for error reporting (synthetic nodes carry `span: None`).
  `CompKind::Bind` carries `scheme: Option<Box<Scheme>>` — the checker's verdict,
  written onto each top-level name-bind by the annotation pass and `None` until
  it runs ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

The checker's verdict rides on the IR too, as **ground** annotations written by
`annotate`. Because the inference pass is unconditional — every evaluated IR is
annotated ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]])
— the slots are not optional: "the checker has not run yet" is not a
representable state.

- `CompKind::Pipeline` is a struct variant
  `{ stages, stage_types: Vec<Ty>, yields: PipeYield }`. `stage_types` holds one
  value type per stage, parallel to `stages`, as typing metadata for the
  structural REPL rather than a transport channel; the elaborator fills it with
  `Unit` placeholders the annotation pass overwrites. `PipeYield { Last, Unit }`
  says what the form hands back — the last stage's reported value, or unit
  because that stage's payload stayed on the byte channel and so never crossed
  the process boundary. It is a *choice of former*, not a route: the checker
  reads the last stage's ground route once and writes the answer down, and no
  route survives into the node. There is nothing per-stage to annotate, because
  every interior edge is an operating-system byte pipe allocated from stage
  position and no rule relates one stage to its neighbour
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]],
  [[map/core/typecheck|typecheck]]).
- `CompKind::Case { scrutinee, arms: Vec<CaseArm> }` is Levy's sum eliminator:
  a `CaseArm` is a tag, the `IrPattern` its payload binds, and the computation
  to run, so the alternatives are a list fixed at parse time and every arm body
  is a node the checker can annotate — an `if` with as many branches as the
  row has labels ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
  An `ArmBody` is `Inline` or `Applied` — the branch the user wrote out, or the
  handler they named applied to the payload. Both are the same branch and are
  typed alike; the distinction exists so a handler that is not a function is
  faulted as an *arm*.
- `CompKind::Capture(Arc<Comp>)` is the kernel half of the checker's one
  payload coercion: run the body, capture its stdout, return those bytes
  exactly. No surface syntax produces it; [[map/core/typecheck|typecheck]]'s
  `annotate` pass inserts it as demand propagation, bound and handed to the
  internal `__decode-captured` builtin for the `String` a value boundary reads
  ([[design/types|types]]). `referenced_names`'s walk descends into it.

The route types live in `core/src/typecheck/route.rs`, a private module of the
checker, and no name from them is reachable from `ir`, `evaluator`, or
`runtime`: the module boundary is the proof that the checked IR is route-free.
Every verdict the evaluator needs is explicit syntax — a `PipeYield`, a
`Capture` node. The elaborator's placeholder yield is `PipeYield::Last`, which
is what an unconstrained route defaults to anyway, and unreachable in practice
since the checker runs before every evaluation.

`CommandName` is the structured head for external dispatch (`Bare` / `Path` /
`TildePath`). `IrPattern = Pattern<Arc<Comp>>` — the same `Pattern` shape as the
AST, but map-pattern defaults are pre-elaborated computations, so no parser syntax
leaks through ([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

`referenced_names` (`pub(crate)`) collects a compiled program's variable and
command-head names in one exhaustive, wildcard-free walk — the use-observation
signal the [[map/core/shell-state|binding-lease ledger]] renews on
([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]).

This shape is what the prelude bake serialises with `postcard`; adding a field to
`CompKind`, `Val`, or `Pattern` invalidates every emitted blob (see
[[map/core|core]] and `core/src/lib.rs`). `docs/SPEC.md` gives the formal CBPV account.
