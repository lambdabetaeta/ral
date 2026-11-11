---
generated_at_commit: e67000a54ec3cbecefa1b52c3c593d70d98bc7f2
generated_at_date: 2026-11-11
covers_paths: [core/src/shell/prelude.ral]
---

# Map: core / shell / prelude

`core/src/shell/prelude.ral` is the standard library, written in ral and baked into the
binary at build time. `build.rs` reads it once, scanning its top-level `let`
bindings into `PRELUDE_EXPORTS` and the `##` summary above each into
`PRELUDE_DOCS` — the table `help` and `explain` print from
([[map/core/builtins|builtins]]), since only values and schemes survive the
bake — and each host's build script serialises the annotated
IR — typechecked by the `typecheck::bake_prelude` pass the runtime also uses,
[[map/core/typecheck|typecheck]] — as a `postcard` blob via
`shell::boot::bake_prelude`, which documents the schema hazard
(`core/src/shell/boot.rs`). The host embeds it with `baked_prelude!`. It
is evaluated once per bake, on a core-only shell, and its bindings are seated
whole in every shell by `BakedPrelude::seat`: a prelude that fails to run
panics rather than seating a partial one.

The prelude holds the *library-level* surface: higher-order list and string
combinators (`for`, `reduce`, `take-while`, `drop-while`, `take`, `drop`,
`enumerate`, `flat-map`, `zip`, `cross`, `nub`, `group-by`, `median-by`,
`words`, `from-words`, `indent`, `repeat`, `pad-left`, `pad-right`,
`extension`, `max`, `min`, `maximum`, `minimum`, `map-lines`, `commands` (an `audit`
report's `` `command `` facts, in trail order), `succeeded` (an `audit`
report's outcome as a `Bool`), …) layered over the directly-registered Rust
builtins (`each`, `map`, `filter`, `fold`, …), and the structured fan-out
over the `gather` and `spawn` control operators: `par` (a bounded parallel
map that keeps input order in bytes and values), `both` and `first-of`
([[decisions/261020_gather|gather]]). These are ordinary
[[design/cbpv|values]] of fixed arity ([[invariants/fixed-arity|fixed-arity]]).

`with-temp-dir` and `with-temp-file` are lexical resource combinators over
`guard { temp-dir }` / `guard { temp-file }`; release runs `rm -rf` / `rm -f`.
The body receives the path; its value and grade are preserved. The path is
removed when the body's extent ends, so workers using it belong inside a
`gather` in that body. Bare `temp-*` remains available for caller-owned
lifetimes ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).
Their `##` summaries are served by `explain`, like every prelude binding's.

Structural equality is the `==` / `!=` expression operator; callbacks use
ordinary lambdas such as `{ |a b| $[$a == $b] }`.

The branching and exception forms `if`, `case`, and `try` are *not* prelude
bindings — they are IR primitives (`CompKind::If` / `Case` / `Try`, see
[[map/core/ir|ir]]) introduced by [[map/core/elaboration|elaboration]], alongside
the seven reserved [[design/control-operators|control operators]]. The prelude
adds the iteration and collection vocabulary on top of that primitive core.

`docs/SPEC.md` §14 lists the prelude exports; a command name's head is classified once
at the surface (`Head` in `core/src/syntax/ast.rs`, [[map/core/syntax|syntax]]).
