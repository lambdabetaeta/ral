---
verified_at_commit: ae2a3f64
verified_at_date: 2026-06-17
anchors: [builtin_registry, CORE_BUILTINS, WATCH_BUILTIN, BuiltinEntry, coreutils_invoke]
---

# Builtins: structure and registration

A builtin is a command implemented in Rust that runs inside the shell process.
`core/src/builtins/` defines them; the question this page answers is how one
declaration stays coherent across the typechecker and the evaluator.

**One macro binds six facets.** `builtin_registry!` (in `builtins.rs`) declares
each builtin as six facets at once, collapsed into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`):

- names;
- computation-type hint;
- fixed arity;
- doc line;
- [[internals/type-inference|type rule]];
- runtime body.

Because the facets are one entry, they cannot drift apart: the arity the checker
enforces is the arity the body assumes ([[invariants/fixed-arity|fixed-arity]]).
Bodies are grouped by concern (`strings.rs`, `collections.rs`, `predicates.rs`,
`fs.rs`, `codecs.rs`, `caps.rs`, `concurrency.rs` for `spawn` / `watch`,
`modules.rs` for `use`, …).

**Registration seeds environments; it is not dispatch.** `register` clones the
builtin bindings into each fresh `Env`, and `is_builtin` / `builtin_names` gate
that seeding. A call is *dispatched* later by the
[[internals/evaluator-machine|evaluator]]: a bare head resolved by
[[internals/surface-syntax|classification]] reaches the builtin body only after
passing the same capability check as any command ([[design/grant|grant]]).

**Bundled coreutils are builtins too.** `builtins/uutils.rs` declares the
vendored tools via `declare_coreutils!` as two `cfg`-gated lists — `cross`
(always on) and `unix` (Unix-only) — emitting one `COREUTILS_TOOLS` slice and a
`coreutils_invoke` dispatcher; `RIPGREP_TOOLS` routes `rg` through
`ral-ripgrep-core`. They run in-process and through the same capability chokepoint
as the structured primitives, which is what lets ral be a
[[invariants/single-binary|single binary]] with no sibling helpers. The call-side
is `runtime/command/uutils.rs`: when ral's sinks are direct-to-fd it dispatches a
resolved head straight into `uutils_invoke`; when a capture buffer, audit tee, or
env/cwd override forbids that, it demotes the call to a single anchorless
`run_pipeline` stage so the bundled tool runs in a stage helper
([[internals/pipeline-execution|pipeline execution]]) rather than mutating this
process's env from a thread.

**Host layers register their own, above core.** The [[map/repl|REPL]] adds the
`_ed-*` editor builtins and exarch adds its resident host atoms; both sit *above*
core and core never inspects them
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]). One
core-implemented builtin registers this way too: `WATCH_BUILTIN` (the public
one-entry wrapper over the private `builtin_watch` / `scheme::watch`) is installed
by the ral host, not seeded into `CORE_BUILTINS`, so an agent host whose streams
are capture buffers simply lacks it
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).

Why these primitives exist and how the set is shaped is
[[design/builtins|builtins]]; which layer any given capability lands in is
[[design/name-resolution|name-resolution]]. See also map
[[map/core/builtins|builtins]]. `docs/SPEC.md` §16, §21.
