---
status: active
---

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
  `RAL_PATH` `find_file` fallback; `source` runs a *file by path*,
  script-relative only. The distinction is the point and stays split.

Folding `use` onto `evaluate_source` also retired its duplicated cycle guard,
`check_source`, and `ScriptContextGuard` block, and gave it the depth bound it
previously lacked. The `Modules` state lost its `cache` field (and its wire
mirror in `WireModules`), and `Modules::return_to` — which propagated only the
cache child→parent — went with it.

See also [[map/core/builtins|builtins]],
[[map/core/capabilities|capabilities]],
`docs/SPEC.md` §10.3–§10.4 (Modules).
