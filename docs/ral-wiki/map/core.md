---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
covers_paths: [core/src/lib.rs]
---

# Map: core (ral-core)

**`core/` is the `ral-core` crate — the language engine — and a host reaches it
through two narrow crate-root seams: a compile-then-typecheck pipeline that turns
source into typed IR, and the framed run door that is the only entry into
evaluation.** It is the bulk of the codebase (~100k lines of Rust); both
binaries, `ral` and [[map/exarch|exarch]], embed it.

`core/src/lib.rs` is the front door. The compilation ladder is *source → tokens →
flat AST → CBPV IR → typed IR*, bundled as two functions: `compile` (parse →
elaborate) and `compile_and_typecheck` (parse → elaborate → typecheck →
`CompileOutcome`). Evaluation is not on the crate root — `evaluate`, `parse`,
`elaborate`, and `Comp` are crate-private, reached by the owning module path when
a host deliberately steps past the seam
([[decisions/260618_after-turn-api-simplifications|after-turn-api-simplifications]]).

## The evaluation seam

Evaluation enters only through the *framed run door* on `Shell`, never the
reduction primitive behind it. A host states policy; core owns resources.

- `Shell::run(RunRequest)` runs one whole `Run`, synchronously and
  runtime-agnostic, returning one flat `RunReport`. Its `Program` is either
  source text or a registered hook applied to first-order arguments —
  `Shell::register_hook` stores compiled hooks by name in the session-lived
  hook table ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).
- Completion is the call returning, never a channel disconnecting, so a deferred
  worker holding a surface clone cannot keep a run from ending
  ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).
- The seam itself is transport-parametric: `transport` is the frame algebra
  (Attach/Dispatch/Event/Control) a front-end speaks, run in-process as the
  identity transport or across a socket to the `engine` child
  ([[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]).
- The reduction host behind the door is crate-private, so a host cannot start an
  unframed evaluation that would foreground or capture against a stale frame.

## host and boot

The crate splits driving a `Shell` from probing the machine it runs on.

- `boot` embeds a `Shell` in a host process: `boot_shell` constructs,
  seeds, loads it from a `BakedPrelude`, and installs the host's `HostSurface` —
  the builtin surface beyond the core set — at construction, so a half-dressed
  production shell is unrepresentable. The prelude is baked ahead of time
  into a schema-less `postcard` blob whose single encode site
  (`boot::bake_prelude_to_out_dir`) and single decode site
  (`boot::BakedPrelude`) sit beside one `cargo:rerun-if-changed` shape-file
  list, so the schema-evolution hazard is one list rather than three that must
  agree ([[decisions/260610_host-embedding-api|host-embedding-api]],
  [[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `host` probes the underlying machine: `git()`, `os_name`/`arch`/`family`,
  wall-clock `now()`, and re-exports of cwd/user/home. exarch's host snapshot is
  a formatter over it.

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
