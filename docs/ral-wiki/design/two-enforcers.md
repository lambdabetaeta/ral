# Two enforcers: why the gate is in-process

A capability decision is made once, in process, then enforced in up to two
places. **The two enforcers are not redundant guards over one piece of ground —
each is authoritative exactly where the other is blind.** The *in-process gate*
is complete for what ral itself dispatches; the *OS sandbox* is necessary for
what escapes that dispatch. Leaving everything to the sandbox would be strictly
weaker, not simpler.

The division falls out of one question: does ral perform the operation, or does a
spawned process perform it on its own? [[internals/capability-enforcement|capability-enforcement]]
narrates *how* each runs; this page argues *why* both exist.

**Why the sandbox cannot be the only enforcer.**

- *It never sees ral's own operations.* The bundled coreutils and the structured
  primitives execute in process — they are not subprocesses, and an OS sandbox
  confines only a child. For everything ral does itself, the in-process gate is
  the one place a check can live ([[internals/builtins-registry|builtins]]).
- *It is coarser than the grant model.* Exec is three-valued (Allow /
  Subcommands / Deny), and the gate matches the runtime argv: `exec: [git:
  [status]]` admits `git status` and denies `git push`. The sandbox renders only
  a path allow/deny list — it gates *which binary*, never *which subcommand*.
  Delegating exec to the OS silently coarsens the grant.
- *It is uneven across platforms.* bwrap has no path-aware exec filter, Windows'
  container is coarse, and `net` is unscopable anywhere. Were the sandbox the
  sole enforcer, `grant` would mean different things on different kernels; the
  in-process gate is the semantic floor that makes `grant` defined by ral, with
  the OS layer tightening further where it can.
- *Its denials are opaque, and it is not free.* An in-process rejection is a
  structured escape carrying an [[design/audit|audit]] fragment — ral names which
  grant denied what, and exarch's attend loop receives it as a value to reason
  about. A sandbox violation is an `EPERM` or a `SIGKILL`, after the fact and
  unattributable. And when no layer restricts fs or net the projection is empty,
  so the per-command sandbox launch is skipped entirely — an unrestricted child
  spawns directly.

**Why the gate cannot be the only enforcer.**

- *It is blind to what a child does on its own.* Once ral spawns `sh`, the gate's
  work is done; `sh` may read, write, and re-exec freely. Confining that requires
  the OS.
- *Hence the asymmetry by dimension.* A spawned child's reads and writes are held
  by the sandbox; `net` has no in-process gate at all, because ral dispatches no
  network operation for the gate to see ([[design/grant|grant]]).
- *Linux exec is the open seam.* ral gates the top-level spawn, but bwrap cannot
  path-filter the child's re-execs, so path-scoped exec confinement is incomplete
  there — a known gap, not a solved case
  ([[decisions/260530_linux-exec-confinement|linux-exec-confinement]]).

The discipline this draws: **the in-process gate is authority over dispatch, not
confinement of children.** Treating it as the latter is the mistake; pairing it
with the sandbox is the design.

This is the boundary [[design/exarch-architecture|exarch]] reuses — a run
evaluates under a grant frame whose two enforcers are exactly these.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (the gate sits where an effect is performed),
[[design/grant|grant]], [[design/scoping|scoping]],
[[design/capability-carriers|capability-carriers]] (this split as a type
distinction — the sharp judge's view vs the blunt guard's checklist); realised in
[[internals/capability-enforcement|capability-enforcement]]. `docs/SPEC.md` §11.
