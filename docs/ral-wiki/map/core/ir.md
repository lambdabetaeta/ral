---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
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

The checker's *mode* verdict rides on the IR too, as **ground** annotations
written by `annotate` ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]).
Because the inference pass is unconditional — every evaluated IR is annotated
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]) — the mode
slots are not optional: "the checker has not run yet" is not a representable state.

- `CompKind::Pipeline` is a struct variant `{ stages, wires: Vec<Wire> }` — one
  `Wire` per stage. The elaborator fills it with `Wire::EMPTY` placeholders that the
  annotation pass overwrites wherever inference visited.
- `CompKind::Bind` carries `rhs_output: ByteMode` — the bound computation's ground
  output mode, the one bit `eval_bind_rhs` reads to decide stdout capture.

A `Wire { input, output }` and `ByteMode { Bytes, Empty }` live in
`core/src/mode.rs`: the two-valued image of a `PipeSpec` with the `PipeMode::Var`
arm removed. The placeholder `Wire::EMPTY` is the same `Var → Empty` default the
annotator writes for an unconstrained wire, so a hypothetical un-annotated path
degrades to all-`Empty` rather than panics — but no such path survives the
unconditional pass. The grounding rule runs once at annotation, so "annotations are
ground" is a fact of the type rather than an invariant the reader must trust — the
lattice's polymorphism lives only in schemes on the checker side, never on a node
the evaluator reads.

`CommandName` is the structured head for external dispatch (`Bare` / `Path` /
`TildePath`). `IrPattern = Pattern<Arc<Comp>>` — the same `Pattern` shape as the
AST, but map-pattern defaults are pre-elaborated computations, so no parser syntax
leaks through ([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

This shape is what the prelude bake serialises with `postcard`; adding a field to
`CompKind`, `Val`, or `Pattern` invalidates every emitted blob (see
[[map/core|core]] and `core/src/lib.rs`). `docs/SPEC.md` gives the formal CBPV account.
