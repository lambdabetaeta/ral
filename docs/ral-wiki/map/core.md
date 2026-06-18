---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [core/src/lib.rs]
---

# Map: core (ral-core)

`core/` is the `ral-core` crate — the language engine, and the bulk of the
codebase (~100k lines of Rust). Both binaries, `ral` and [[map/exarch|exarch]],
embed it.

`core/src/lib.rs` is the front door. It re-exports the whole public surface and
states the pipeline as two functions: `compile` (parse → elaborate) and
`compile_and_typecheck` (parse → elaborate → typecheck → `CompileOutcome`).
Hosting a `Shell` is `host::boot_shell` over a `BakedPrelude`, and the prelude
bake — a schema-less `postcard` blob with its `cargo:rerun-if-changed`
shape-file list (`ir.rs`, `syntax/ast.rs`, `mode.rs`, `typecheck/ty.rs`,
`typecheck/scheme.rs`) — is owned in one place, `host.rs`, so the
schema-evolution hazard is a single list rather than three that must agree
([[decisions/260610_host-embedding-api|host-embedding-api]]).

## The pipeline, by judgment

Source text flows down a fixed ladder; each rung is a subsystem page.

- [[map/core/syntax|syntax]] — lexer and parser, producing the flat surface AST
  (`core/src/syntax/`).
- [[map/core/elaboration|elaboration]] — lowers surface forms into CBPV IR
  (`core/src/elaborator.rs`).
- [[map/core/ir|ir]] — the `Val` / `Comp` intermediate representation
  (`core/src/ir.rs`).
- [[map/core/typecheck|typecheck]] — Hindley–Milner inference with row types
  (`core/src/typecheck/`), the sole mode-inference engine; the mode lattice in
  `mode.rs` is covered by the same page.
- [[map/core/evaluator|evaluator]] — the CBPV machine: trampoline, scope frames,
  matching, audit (`core/src/evaluator/`).
- [[map/core/runtime|runtime]] — the command/pipeline/transport machinery the
  machine dispatches into, and the shared re-exec'd-child eval runner
  (`core/src/runtime/`, `core/src/child_eval.rs`).

## Authority, plumbing, surface

- [[map/core/capabilities|capabilities]] — the dynamic capability stack and the OS process
  sandbox (`core/src/capability/`, `core/src/sandbox/`).
- [[map/core/io-process|io-process]] — byte streams, signals, process groups, the Stream
  protocol (`core/src/io/`, `core/src/process/`, `core/src/stream.rs`).
- [[map/core/builtins|builtins]] — Rust-implemented commands and bundled coreutils/grep
  (`core/src/builtins/`).
- [[map/core/shell-state|shell-state]] — runtime values and the `Shell` interpreter state
  (`core/src/types/`).
- [[map/core/transport|transport]] — the serde mirror and wire envelope that carry a
  shell across a re-exec (`core/src/serial.rs`, `subprocess.rs`).
- [[map/core/diagnostics|diagnostics]] — source locations and error rendering
  (`core/src/source.rs`, `diagnostic.rs`, `ansi.rs`).
- [[map/core/prelude|prelude]] — the embedded `prelude.ral` standard library
  (`core/src/prelude.ral`).

## Siblings

[[map/repl|repl]] is the `ral` binary over this engine; [[map/exarch|exarch]] is the agent
embedding it.

_The formal account is `docs/SPEC.md`; the design rationale lives in the
[[AGENTS|durable layer]]. These pages point at code, they do not restate it._
