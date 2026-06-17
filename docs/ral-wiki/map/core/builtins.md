---
generated_at_commit: ae2a3f64
generated_at_date: 2026-06-17
covers_paths: [core/src/builtins/, core/src/builtins.rs]
---

# Map: core / builtins

`core/src/builtins/` are the commands implemented in Rust that run inside the
shell process. `builtins.rs` holds the `builtin_registry!` macro: each entry
binds its facets at once — `names`, fixed `arity`, [[map/core/typecheck|type
rule]] (`ty`), `doc` line, and runtime body (`call`) — into the `CORE_BUILTINS`
static (`&[BuiltinEntry]`), so the facets cannot drift apart, and the macro
asserts a `Sig` rule's structural arity agrees with the written `arity`. The
type-rule facet is a `BuiltinTypeRule` of two arms: `Sig` (a command signature)
or `Scheme` (a first-class polytype). The streaming reducer `fold-lines`
registers as an ordinary `Scheme` whose factory bakes the reducer boundary
directly ([[map/core/typecheck|reducer_spec]]); there is no separate reducer
arm. Fixed arity is the registry's job ([[invariants/fixed-arity|fixed-arity]]).
`is_builtin` / `builtin_names` gate seeding; `register` clones the prelude
bindings into each fresh environment. One builtin sits *outside* the macro: the
public `WATCH_BUILTIN` (`&[BuiltinEntry]`) wraps the still-private
`concurrency::builtin_watch` / `scheme::watch` so a host with a durable stdout
sink installs it while an agent host omits it
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).

Bodies are grouped by concern, one submodule each:

- `strings.rs`, `collections.rs`, `predicates.rs`, `fs.rs`, `codecs.rs`;
- `shell.rs` — `cd`, `alias` / `unalias`;
- `concurrency.rs` — `spawn` / `watch` and the handle verbs `await` / `poll` /
  `race` / `cancel` (builtins under their bare names; `par` and the `is-done`
  predicate are prelude code over them, not builtins). All but `watch` seed
  through `CORE_BUILTINS`; `watch`'s implementation lives here too but is
  installed by the host via `WATCH_BUILTIN`, not core. On completion a
  block's buffers drain *once* into a cached `CompletedHandle { stdout, stderr,
  outcome }` ([[map/core/shell-state|types/value.rs]]); the eliminators project that
  one settle. `try_settle` is the shared non-blocking sample (cached outcome, else a
  `try_recv` completed through `complete_handle`; a `Disconnected` receiver — a
  panicked worker — settles as the same failure `await` reports, so `poll`/`race`
  see a finished block rather than spinning). `await`/`race` `project_completed` the
  outcome to `{value, stdout, stderr}`, re-raising `` `err ``; `poll` is total,
  wrapping it as `` `settled `` `{stdout, stderr, outcome: `ok/`err}` (the `` `err ``
  payload built through the shared `evaluator::scope::error_record`, the record
  `try` hands its handler) or `` `pending ``, and leaving `last_status` at 0 since
  the block's status is data. `await` and `poll` gate first on `ensure_live`, the
  cancelled pre-check
  ([[decisions/260615_poll-total-failed-arm|the settle decision]]).
  A detached worker hangs under the durable session root, not the turn's
  foreground scope, so a foreground cancel never reaps it; `await` shares
  `race`'s cancel-aware wait loop (`wait_first_settled`), so a deadline unwinds
  the wait while the root-scoped worker survives, and under a frame lifetime
  ceiling `spawn` arms the worker's scope with the shared `process::reaper`
  ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
  A worker runs its thunk on a fresh
  `std::thread` via `Shell::spawn_thread` ([[map/core/shell-state|shell-state]]),
  which inherits a snapshot of the parent's mobile state; the body is evaluated
  *directly* with `eval_comp(.., Tail::Yes)` under a child scope, deliberately
  bypassing the `eval_top_level` / `eval_block` boundary, because the worker's own
  `Shell` is the only one its bindings touch and they die with the thread. No
  confined re-exec is attempted from a worker — the OS sandbox already wraps the
  whole process — yet a forced block *inside* the worker still meets the standard
  boundary rule, so a `spawn` under a `grant` cannot escape it. A `Handle` is a
  resident, process-local reference: it cannot cross the sandbox IPC boundary, so
  returning one from a confined evaluation raises the wire diagnostic *"cannot
  return a handle from sandboxed evaluation"* (`core/src/serial.rs`) rather than a
  generic failure ([[internals/capability-enforcement|capability-enforcement]]);
- `modules.rs` — the cacheless `use` / `source` loader. `evaluate_source` is
  the shared guarded parse + elaborate + evaluate core (cycle stack, depth
  bound, `ScriptContextGuard`); `use` is a scope-projecting wrapper over it,
  `source` evaluates into the caller's scope. Module loads carry no cache, so
  the guards keep re-evaluation terminating — see
  [[decisions/260606_cacheless-module-loader|cacheless-module-loader]];
- `misc.rs` — including `surface`, which forwards a tagged variant to the host's
  [[map/core/shell-state|`SurfaceSink`]] and is the identity under a bare REPL;
- `util.rs` — shared helpers, JSON coercion.

The capability `Value`-map decoder is *not* a builtin: it lives beside the
authority layer in `capability/decode.rs` (`decode_capability_map`), consumed by
the `grant` control operator (`evaluator/scope.rs`) and the `--capabilities`
ceiling (`capability/load.rs`) — see [[map/core/capabilities|capabilities]],
[[design/grant|grant]].

Why a capability lands in one of these layers rather than another — builtin vs.
coreutil vs. prelude vs. control operator — is [[design/name-resolution|design: name-resolution]];
what a builtin *is* and the shape of the set is [[design/builtins|design: builtins]];
the `from-X`/`to-X` byte↔value typing in `codecs.rs` is [[design/codecs|design: codecs]].

## Bundled coreutils and ripgrep

`uutils.rs` declares the bundled tools as two parallel lists via
`declare_coreutils!`: `cross` (always on under the `coreutils` feature) and
`unix` (additionally under `coreutils-unix-only`, `cfg(unix)`-gated). It emits
one merged `COREUTILS_TOOLS` slice and a `coreutils_invoke` dispatcher.
`RIPGREP_TOOLS` (`["rg"]`) routes through `ral-ripgrep-core`. These run in-process
and go through the same capability chokepoint as everything else — part of why
ral is a [[invariants/single-binary|single-binary]]. The `grep` cargo feature separately backs
the `re-*` regex string builtins.

The [[map/core/runtime|runtime]]'s `command/uutils.rs` is the call-side that
dispatches a resolved head into `coreutils_invoke`. `docs/SPEC.md` §21 covers the
single-binary tool surface.
