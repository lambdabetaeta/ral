---
generated_at_commit: 34ae1424
generated_at_date: 2026-10-25
covers_paths: [core/src/lib.rs, core/src/shell.rs, core/src/host.rs, core/src/host/, core/src/fact.rs, core/src/fact/]
---

# Map: core (ral-core)

**`core/` is the `ral-core` crate — the language engine — and a host reaches it
through two narrow crate-root seams: a compile-then-typecheck pipeline that turns
source into typed IR, and the framed run entry that is the only way into
evaluation.** It is the bulk of the codebase (~100k lines of Rust); both
binaries, `ral` and [[map/exarch|exarch]], embed it.

`core/src/lib.rs` is the crate root. The compilation ladder is *source → tokens →
flat AST → CBPV IR → typed IR*, bundled as `compile::compile_and_typecheck` (parse →
elaborate → typecheck, failing with a `CompileError` of `Parse` or `Types`)
and, crate-privately, `compile`
(the same ladder without the checker), both returning a `Toplevel` — a sequence of phrases
(`Define`/`Run`) run by `shell::evaluator::run_phrases`, which replaced the
flat `Comp`-rooted top level. Evaluation is not on the crate root — `parse`,
`elaborate`, and `Toplevel` are crate-private, reached by the owning module path
when a host deliberately steps past the seam
([[decisions/260618_after-turn-api-simplifications|after-turn-api-simplifications]]).

Reach is enforced rather than intended: an item is `pub` because something
outside the crate names it, `pub(crate)` otherwise, so `dead_code` can name an
internal whose last caller went away
([[decisions/260909_pub-crate-by-default|pub-crate-by-default]]). Core's own
integration tests link `ral-core` as an external crate and reach internals
through one gated module, `core::test_access`, for cross-component assertions.
Representation-local tests live beside their implementations: builtin registry
properties in `shell/builtins/tests.rs`, stored-scheme reseeding in
`typecheck/session_tests.rs`, and scheme formatting in `ty/fmt.rs`.
Integration tests' engine reads go through `IdentityTransport::inspect`, the
one `test-util`-only read of an identity engine's shell; core's unit tests boot
engines through `engine::testkit` and prelude shells through
`test_helper::fresh_shell`.

## The evaluation seam

Evaluation enters only through the *framed run entry* on `Shell`, never the
reduction primitive behind it. A host states policy; core owns resources.

- `Shell::run(RunRequest)` runs one whole `Run`, synchronously and
  runtime-agnostic, returning one flat `RunReport`. Its `Program` is either
  source text or a registered hook applied to first-order arguments —
  `Shell::register_hook` stores compiled hooks by name in the session-lived
  hook table ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).
- Completion is the call returning, never a channel disconnecting, so a deferred
  worker holding a surface clone cannot keep a run from ending
  ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).
- The protocol itself is transport-parametric: `protocol` is the frame algebra
  (Attach/Dispatch/Event/Control) a front-end speaks, run in-process as the
  identity transport or across a socket to the `engine` child
  ([[map/core/engine-protocol|engine-protocol]]).
- The reduction host behind the entry is crate-private, so a host cannot start an
  unframed evaluation that would foreground or capture against a stale frame.

## The shell subjects

**`core/src/shell.rs` groups shell code by subject, not by dependency layer.**

- `shell::runtime` owns runtime values and interpreter state; static types stay in
  `ty` and inference in `typecheck`.
- `shell::evaluator` is the CBPV machine; `shell::exec` owns command dispatch,
  pipelines and sink scopes. The evaluator passes `pipeline::StageEval` callbacks
  into `exec`, so stage threads re-enter evaluation without importing the machine.
- `shell::grant` decodes runtime grant values and adapts spawn grants;
  substantive freezing and authored resolution stay in `authority::grant`.
- `shell::builtins`, `shell::run`, `shell::boot` and `shell::load` own primitives,
  the framed entry (including `shell/run/report.rs`), boot and loading;
  `shell/prelude.ral` is the embedded library.
- `seed`, `role`, `protocol`, `carrier` and `engine` stay at the crate root,
  alongside the other non-shell subjects.

**`authority`, `fact`, `host`, `io`, `own`, `path` and `process` cannot reach
`shell`, directly or transitively, in production.** Their intentional lower
cycles are permitted: this is a
[[invariants/core-below-shell|boundary, not a DAG]].

## host and boot

The crate splits driving a `Shell` from probing the machine it runs on.

- `shell::boot` embeds a `Shell` in a host process. `Shell::root(runtime)` constructs over a `Runtime`, the surface a
  host chooses through `HostSurface::runtime` or `boot_runtime`, so every
  shell chooses its surface through one of two constructors:
  `HostSurface::shell` (core's `CORE_SETS`, then the host's sets, no env, no
  prelude: a loader's scaffold) or `boot_shell`, which adds the host env and
  seats a `BakedPrelude` whole (`BakedPrelude::seat`). A half-dressed
  production shell is unrepresentable, and `HostSurface::manifest` hands the
  checker the same list. The prelude is baked ahead of time
  into a schema-less `postcard` blob whose single encode site
  (`shell::boot::bake_prelude`, returning the blob for a host's build script to write) and single decode site
  (`shell::boot::BakedPrelude`) sit together, and the build-dependency edge from each
  host to `ral-core` reruns the bake, so the schema-evolution hazard has one guard rather than three that must
  agree ([[decisions/260610_host-embedding-api|host-embedding-api]],
  [[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).
- `host` probes the underlying machine: `git()`, `os_name`/`arch`/`family`,
  wall-clock `now()`, and re-exports of cwd/user/home. It is also the one reader
  of the host's environment (`var`, `var_os`, `vars_os`, `temp_dir`), and of the
  XDG bases (`xdg`, with the account's home as the program's own fallback).
  `host/env.rs` owns the persistent environment overlay, exported as
  `host::{EnvVars, EnvVarsIter}`, shared by shell state, paths and launches.
  Its test-only `host/env/test_support.rs` serialises temporary host-environment
  mutations. exarch's host snapshot is a formatter over it.
- `fact/fs.rs` owns the typed `DirEntry` and `FileInfo` query records and their
  pure construction from metadata; builtins and the checker share these schemas.
- `fact/observation.rs` owns observation data, including `AuditIo`, the captured
  byte prefixes and totals a command fact consumes; collection belongs to runtime.
- `own` is the files a program touches on its own account, outside any shell:
  `OwnDir` (what it keeps), `UserFile` (what the user writes at its places) and the
  user-named `read_named`, `open_named`, `kind_named` and `create_named` (banned,
  so each caller is a typed site that says why the name is the user's), over the
  walk and one authority, with three authority-mint sites but no syscall sites
  ([[map/core/capabilities|capabilities]],
  [[decisions/261022_own-files|own-files]]).

## The pipeline, by judgment

Source text flows down a fixed ladder; each rung is a subsystem page.

- [[map/core/syntax|syntax]] — lexer and parser, producing the flat surface AST
  (`core/src/syntax/`).
- [[map/core/elaboration|elaboration]] — lowers surface forms into CBPV IR
  (`core/src/elaborator.rs`).
- [[map/core/ir|ir]] — the `Val` / `Comp` intermediate representation
  (`core/src/ir.rs`).
- [[map/core/ty|ty]] — the type language as data, below the IR
  (`core/src/ty.rs`, `core/src/ty/`).
- [[map/core/typecheck|typecheck]] — Hindley–Milner inference with row types
  (`core/src/typecheck/`), the sole inference engine, including the grade that places capture
  (`CompTy::Return(Grade, _)`).
- [[map/core/evaluator|evaluator]] — the CEK machine over computation closures:
  focus, frame stack, matching, audit (`core/src/shell/evaluator/`).
- [[map/core/runtime|exec]] — command dispatch, pipelines and sink scopes the
  machine dispatches into (`core/src/shell/exec/`).

## Authority, plumbing, surface

- [[map/core/capabilities|capabilities]] — the dynamic capability stack and the OS process
  sandbox (`core/src/authority/`: resolved policy and judgments in `policy/`,
  authored resolution and host system-read discovery in `grant/`, enforcement in `guard/`
  and `sandbox/`; runtime decoding and spawn adaptation in `shell/grant/`).
- [[map/core/io-process|io-process]] — byte streams, terminal, signals, process
  groups (`core/src/io/`, `core/src/process/`).
- [[map/core/builtins|builtins]] — Rust-implemented commands and bundled coreutils/grep
  (`core/src/shell/builtins/`).
- [[map/core/shell-state|shell-state]] — runtime values and the `Shell` interpreter state
  (`core/src/shell/runtime/`).
- [[map/core/transport|transport]] — the serde mirror and wire envelope that carry a
  shell across a process boundary (`core/src/seed.rs`, `seed/table.rs`, `io/frame.rs`).
- [[map/core/diagnostics|diagnostics]] — source locations and error rendering
  (`core/src/source.rs`, `diagnostic.rs`, `compile.rs`, `io/terminal.rs`, `io/ansi.rs`).
- [[map/core/prelude|prelude]] — the embedded `prelude.ral` standard library
  (`core/src/shell/prelude.ral`).

## Siblings

[[map/repl|repl]] is the `ral` binary over this engine; [[map/exarch|exarch]] is the agent
embedding it.

_The formal account is `docs/SPEC.md`; the design rationale lives in the
[[AGENTS|durable layer]]. These pages point at code, they do not restate it._
