---
generated_at_commit: 95449d4
generated_at_date: 2026-08-10
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
  `{ stages, stage_types: Vec<Ty>, final_route: GroundRoute }`. `stage_types`
  holds one value type per stage, parallel to `stages`, as typing metadata for
  the structural REPL rather than a transport channel; the elaborator fills it
  with `Unit` placeholders the annotation pass overwrites. `final_route` is one
  route for the whole node — the checker's verdict on the last stage, and the
  only thing that decides whether the pipeline reports its helper's returned
  value. There is nothing per-stage to annotate, because every interior edge is
  an operating-system byte pipe allocated from stage position and no rule
  relates one stage to its neighbour
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]],
  [[map/core/typecheck|typecheck]]).
- `CompKind::Capture(Arc<Comp>)` is the checker's one payload coercion: run the
  body, capture its stdout, decode it as a `String` value. No surface syntax
  produces it; [[map/core/typecheck|typecheck]]'s `annotate` pass inserts it as
  demand propagation. `referenced_names`'s walk descends into it.

`GroundRoute { Value, Bytes }` lives in `core/src/route.rs`, the resolved image
of the checker's `PayloadRoute` with the `Var` arm removed — so "annotations are
ground" is a fact of the type rather than an invariant the reader must trust.
The elaborator's placeholder is `GroundRoute::Value`, which is what an
unconstrained route defaults to anyway, and unreachable in practice since the
checker runs before every evaluation. Route polymorphism lives only in schemes
on the checker side, never on a node the evaluator reads.

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
