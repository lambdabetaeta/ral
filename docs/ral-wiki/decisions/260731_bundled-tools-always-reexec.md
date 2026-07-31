---
status: active
---

# Bundled tools always run as re-exec'd children

**Every bundled coreutils/diffutils/ripgrep invocation is a child process whose
image is ral itself** — `ral --ral-bundled-tool <tool>` — through the same
build/confine/spawn/wait machinery as a host executable. The inline in-process
placement is removed; child placement is not the model with an optimisation
beside it, it is the whole mechanism.

## Context

[[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]
made a bundled command an executable image with two placements: the child spawn
as the model, plus an inline `uumain` call kept for the clean-terminal case,
admitted by a gate requiring terminal stdio, no redirects, no env overrides, no
logical/process cwd divergence, and no sandbox projection.

The env conjunct made that gate unsatisfiable in any booted shell.
`Shell::seed_default_env_vars` — run by `boot_shell` for every front end —
installs `HOME`/`USER`/`PATH`/`SHLVL`/`OS_*` into `env_overrides`
([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]), so
"no env overrides" held only for the unseeded shells unit tests construct. The
optimisation the inline placement existed to preserve — a cheap `ls` at an
interactive Windows prompt — never ran; what the placement actually consisted
of was a process-global mutex, panic isolation, a defensive cwd save/restore,
two silent I/O doors, and a per-call gate, all guarding a path only tests could
reach. A gate whose seeded overrides are mostly *equal* to the host env could
be loosened to an agreement check, but `SHLVL` (incremented past the host's)
and the `OS_*` compile-time facts disagree with the process env by
construction, so a reachable inline placement would need a semantic exemption
list — more mechanism, for a placement the runtime had been living without.

## Decision

Adopt the alternative 260616 rejected: **always self-reexec**.

- `command::run` treats `ExecImage::BundledTool` exactly as `ExecImage::Host`:
  vet, build, confine, spawn under the canonical pgid, reap — and one exec door
  at the wait, shared with host externals.
- The admission gate, the inline runner, its mutex, and the cwd-agreement
  predicate are deleted; the io-door allow set shrinks by the two inline cwd
  doors.
- uucore's process-global exit-code cell is read only inside the single-job
  child (`try_run_bundled_tool`), where the process is the job, so no
  cross-thread serialisation exists anywhere.

## Consequences

- One placement, one exec door: `command::run` emits every standalone exec
  event; `detach` remains the spawn-time door
  ([[map/exarch/io-surface|io-surface]]).
- The parent process never runs third-party `uumain` code, so its fds, env,
  cwd, and panic state are never exposed to a bundled tool.
- The Windows process-creation cost that motivated the inline placement is
  accepted — it was already being paid on every real invocation.
- Everything else in 260616 stands: the `ExecImage` representation, the hidden
  multicall entrypoint, the pipeline byte-stage/value-edge rules, and the audit
  story.

See also [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]],
[[map/core/runtime|map: runtime]], and [[map/core/builtins|map: builtins]].
