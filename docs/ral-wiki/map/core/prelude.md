---
generated_at_commit: fc49779
generated_at_date: 2026-07-23
covers_paths: [core/src/prelude.ral]
---

# Map: core / prelude

`core/src/prelude.ral` is the standard library, written in ral and baked into the
binary at build time. `build.rs` reads it, scans its top-level `let` bindings
into `PRELUDE_EXPORTS`, and the consumer build scripts serialise the annotated
IR — typechecked by the same `bake_prelude` path the runtime uses,
[[map/core/typecheck|typecheck]] — as a `postcard` blob (the schema hazard
documented in `core/src/lib.rs`). It
is evaluated once per process and its bindings are cloned into every fresh
environment ([[map/core/builtins|register]]).

The prelude holds the *library-level* surface: higher-order list and string
combinators (`for`, `reduce`, `take-while`, `drop-while`, `take`, `drop`,
`enumerate`, `flat-map`, `zip`, `cross`, `nub`, `group-by`, `median-by`,
`lines`, `words`, `indent`, `map-lines`, `defer`, the `stream-*`
eliminators, …) layered over the directly-registered Rust builtins (`each`,
`map`, `filter`, `fold`, …). These are ordinary [[design/cbpv|values]] of fixed
arity ([[invariants/fixed-arity|fixed-arity]]).

The branching and exception forms `if`, `case`, and `try` are *not* prelude
bindings — they are IR primitives (`CompKind::If` / `Case` / `Try`, see
[[map/core/ir|ir]]) introduced by [[map/core/elaboration|elaboration]], alongside
the five reserved [[design/control-operators|control operators]]. The prelude
adds the iteration and collection vocabulary on top of that primitive core.

`docs/SPEC.md` lists the prelude exports; a command name's head is classified once
at the surface (`Head` in `core/src/syntax/ast.rs`, [[map/core/syntax|syntax]]).
