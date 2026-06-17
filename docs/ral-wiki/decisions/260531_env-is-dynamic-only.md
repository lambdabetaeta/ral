---
status: active
---

# The environment is dynamic state, read through `$env`

Environment variables are **ambient authority, not lexical data**:

- they flow to child processes;
- they are overlaid by `within [env: …]` over a block's whole dynamic extent;
- they are attenuable.

Their single home is `context.env_overrides` (`EnvVars`), which rides the dynamic
[[design/scoping|`Context`]] subtree through `inherit_from` / `spawn_thread` and
serialises across the re-exec boundary (`WireContext`). ral code reads them as
`$env[KEY]`; tilde expansion is `$env[HOME]` and bare command resolution falls
back to `$env[PATH]` (`docs/SPEC.md`).

The lexical scope (`Env`) holds immutable data bindings and does **not** flow
through `inherit_from`, so it could never carry the environment to a child —
which is exactly why `apply_env` reads `env_overrides`, never `scope`. The
startup seeding of `HOME`/`USER`/`PATH`/… into `Env` was therefore a read-side
convenience for a bare `$HOME`, duplicating keys that `within [env: …]` only
ever updates in `env_overrides`; the two views drifted
([[decisions/260530_env-overrides-scope-overlap|env-overrides-scope-overlap]]).

Resolution: seed the well-known variables into `env_overrides` only. A bare
`$HOME` is now an undefined variable; the environment is reached through
`$env[KEY]` alone. With one authority, a `within [env: …]` overlay is observed
identically by `$env`, `~`, the child-process environment, and PATH resolution.
This sharpens the [[design/scoping|lexical-data / dynamic-authority]] split
rather than straddling it.

Realised in `core/src/types/shell/init.rs` (`seed_default_env_vars`).
See also [[map/core/shell-state|shell-state]].
