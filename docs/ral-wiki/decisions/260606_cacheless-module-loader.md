---
status: active
---

> **Amended 2026-11-12 — Static linking and closed units.** This amendment
> is authoritative over the loading, scope and path mechanisms below.
> `use` takes one written path and is linked when its importer is checked.
> Every check reads the module again; every evaluation runs the linked body
> again. There is no module cache or runtime file read.
>
> The exact written path is the sole candidate: `~` and `xdg:` heads expand,
> absolute paths remain absolute, and relative paths anchor at the containing
> file (the session cwd for a source without a file). ral reads no `RAL_PATH`.
> Check-time session environment and grant govern resolution and reading;
> an earlier `cd` or surrounding `within`/`grant` in the unit cannot alter
> the read. The runtime lexical grant still governs the module's effects.
>
> A module is checked against Σ (natives and prelude) and run under Σ,
> without session or importer bindings. Its public definitions must have
> decided schemes: after successful `solver.close()`, export judgment reads
> `&Solution` and pending IR before readback settles schemes, retaining each
> boundary's call span. Any public type reaching a weak variable, data
> included, is refused. Accepted public schemes instantiate in the importer;
> an importing `let` generalises normally.
>
> Elaboration's loader links `Use(Arc<Module>)`, carrying checked `Toplevel`
> and origin. A recursive check-time origin chain detects cycles and bounds
> depth to 100; it is an argument, not shell state or seed data. Missing,
> denied, ill-typed, cyclic and too-deep modules refuse the importer before
> evaluation and cannot be caught by `try`. Read denials remain in the
> dispatch trail at the `use`, including on static refusal.
>
> Host loading has two doors: only the rc and exarch's agent library use
> `extend_session`; plugins, profiles, exarch's config and net policy use
> `evaluate_closed`, checked and run under Σ with local helper definitions.
> `run_module` shares that closed execution and projects public names.
> A plugin key without a separator names `<config>/ral/plugins/NAME.ral`;
> any other key names one written path, with no search or extension fallback.
> `--check` performs the run's full engine boot, applies profiles and the
> recursion limit, then checks without evaluating through `Program::Check`.
>
> See [[map/core/ir|IR]], [[internals/compilation-ladder|compilation ladder]],
> [[map/core/diagnostics|diagnostics]] and [[map/repl/plugins|plugins]].

# Module loads carry no cache; share `evaluate_source`

`use` once kept a per-process result map keyed by canonical absolute path,
returned on every subsequent load of the same file. **The decision: module
loads carry no cross-session cache — `use` and `source` re-read and
re-evaluate the file on every call** — choosing freshness over a memoisation
that, in a shell, buys nothing.

## Why the cache was not load-bearing

The cache was keyed by canonical path with no invalidation hook, so the only
thing it changed was *when* a file's bindings were observed — and every
rationale for keeping that around dissolves in a shell.

- *Staleness was the cost it imposed.* In a long-lived REPL, editing a module
  and re-`use`ing it served the old bindings; the file's side effects ran only
  on first load. The escape hatch was "restart the process" — i.e. discard the
  cache the cache existed to preserve.
- *The cross-import dedup never fires across processes.* Scripts are separate
  processes with independent caches, so a module imported by two scripts is
  loaded twice regardless. The cache could only dedup *within* one process.
- *Within one process the win is rare and cheap to forgo.* The REPL is
  human-paced — re-parsing a small module between prompts is imperceptible — and
  an in-run diamond (one run `use`ing the same module along two paths) is
  uncommon. Where it matters, a caller binds the result once with `let`.

## The guards are retained and now load-bearing

Cacheless re-evaluation must still terminate. The cycle stack
(`modules.stack`) and depth bound (`modules.depth`, `MAX_SOURCE_DEPTH`) in
`evaluate_source` are kept: a file that `use`s or `source`s itself is rejected
as a circular dependency, and unbounded nesting trips the depth limit. With no
cache absorbing the second visit, these are the only thing standing between a
self-referential module and divergence.

## `use` becomes a wrapper over the shared core

`evaluate_source` is the single guarded parse + elaborate + evaluate core
(it is also what the capability-file loader uses). `use` and `source` now share
it and a `read_and_normalize` helper (`check_fs_read` + read + line-ending
normalisation, with the same not-found / permission / other messages for both
verbs), differing only in two intentional axes:

- *Scope and return.* `source` evaluates into the caller's scope — its bindings
  leak — and yields the file's terminal value; `use` pushes a fresh scope,
  routes through `evaluate_source`, and projects the top scope into a Map
  (dropping `_`-prefixed names).
- *Path policy.* `use` resolves a module *by name* — script-relative, then a
  `RAL_PATH` fallback (`find_on_ral_path`); `source` runs a *file by path*,
  script-relative only. The distinction is the point and stays split.

Folding `use` onto `evaluate_source` also retired its duplicated cycle guard,
`check_source`, and `ScriptContextGuard` block, and gave it the depth bound it
previously lacked. The `Modules` state lost its `cache` field (and its wire
mirror in `WireModules`), and `Modules::return_to` — which propagated only the
cache child→parent — went with it.

See also [[map/core/builtins|builtins]],
[[map/core/capabilities|capabilities]],
`docs/SPEC.md` §10.3–§10.4 (Modules).
