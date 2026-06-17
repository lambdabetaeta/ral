---
status: superseded
---

# Typed wrappers for shell-state flow

A design study on using the type system to prevent a class of shell-state flow
bugs — stale working directory, a field mutated in a child but never returned to
the parent. It proposes three moves:

- give moved state typed wrappers (`Borrowed<T>`, `Returned<T>`, plus the
  vocabulary Bound / Ambient for the other cases);
- tighten environment-variable boundaries so process-owned keys are not cached;
- audit direct `std::env::current_dir` call sites.

The guiding principle is restraint: **use types only where the compiler can
prevent a realistic mistake.** `Bound` is usually just `Clone`; `Ambient` is
absence; the wrappers earn their place only at the boundaries where a real bug
(PWD staleness, forgotten field return) would otherwise slip through.

A direction, not a committed implementation; the env-key question overlaps the
open thread in [[decisions/260530_env-overrides-scope-overlap|env-overrides-scope-overlap]].

**Resolved without the wrappers.** The two boundaries the study worried about
were closed by other means, leaving the wrappers no realistic mistake to
prevent:

- the env-key boundary moved to dynamic scoping in
  [[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]] — `env_overrides`
  is the sole authority and `PWD` / `OLDPWD` live only on `context.cwd`, so no
  process-owned key is cached where it could drift stale;
- the direct `std::env::current_dir` call sites were audited down to a few
  annotated, justified uses (`core/src/path.rs`, the uutils and pipeline-helper
  cwd snapshots), each carrying an explicit clippy exemption and a reason.

So `Borrowed<T>` / `Returned<T>` / `Bound` / `Ambient` were never built: by the
page's own restraint principle — *types only where the compiler can prevent a
realistic mistake* — once those boundaries held, the wrappers did not earn their
place.

Full study:
`dev/docs/260430_flow.md`. See also [[map/core|core]], [[design/scoping|scoping]].
