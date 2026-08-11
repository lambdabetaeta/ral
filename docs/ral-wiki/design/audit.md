# Audit

**ral records execution as a flat, lexically-scoped trail.** The `audit`
operator runs its body and returns, alongside the body's value, the trail of
what happened — commands, redirect reads and writes, capability checks,
worker births — so a run can be inspected after the fact. It is one of the
five [[design/control-operators|control operators]] for the same reason as
the others: the trail it threads lives in `Shell` state, not in any value the
body could construct. Every fact is one vocabulary, `Observation`
(`core/src/types/observation.rs`) — the same one the surface rail and
`--audit`'s JSON project, and the same one a host author speaks (exarch's
desk records its committed acts as `Observed::Act`, host-side, into a
per-call fragment of its own rather than the engine's trail) — so a command,
a write, a read, a capability check, and a worker's birth are one shape
apiece rather than five private ones.

**Collection is lexical, not temporal, and the trail itself is flat.**

- Each scope-producing site — `audit`, and the other operators `within` /
  `grant` / `guard` / `try` — owns the trail its body produces: it opens (or
  inherits) collection, and everything the body observes lands in that same
  flat list. None of them builds an observation of its own; they are
  collection boundaries, not entries in the trail.
- Source nesting decides which collection an observation lands in, regardless
  of which process or thread produced it.
- A process boundary — the OS-sandbox child a `grant` re-execs into
  ([[design/grant|grant]]), a pipeline-stage helper — only *transports* its
  fragment back to the owning evaluator, which merges it into the surrounding
  trail; the boundary never decides structure.
- A delimiter's lifecycle is scope-shaped, not one-shot: opening either
  installs a trail or finds one already open, and whichever happened decides
  what closing does. The delimiter that installed the trail drains and closes
  it on every exit, panics included, so the next dispatch starts from nothing;
  a delimiter that found one already open reads back only its own suffix and
  leaves the trail intact for its opener to close later. Nobody but the
  opener closes — that discipline is what keeps a `try` or an `audit { }`
  from leaving the trail standing open for the rest of the session.

So a sandboxed `grant { … }` merges its body's commands into the nearest open
trail rather than losing them at the process boundary, and a transported
fragment stays self-describing without needing a parent to interpret it.

Each observation is self-describing about who and how it happened:

- every observation carries the `principal` in force where it was recorded, so
  the trail records *who* as well as *what*, and a transported fragment still
  names its actor;
- an observation carries only its own kind's fields — a command's `argv`, a
  capability check's `resource` / `decision` — never a handler frame or
  capability map, and never another observation's fields;
- tail-recursive iteration adds no wrapper: a loop contributes one flat run of
  observations, not a chain as deep as the iteration count, matching how
  [[design/scoping|dynamic scope]] persists across tail calls.

What builds up the trail is itself scoped:

- plain execution records nothing unless a host asks: a dispatching host
  (exarch's tool call) may open its own delimiter over the whole run —
  `Run.trail: Some` in the transport protocol — and get the extent's trail
  back on the `Report`, each observation in the shared `Observation` map
  shape, with an `` `opaque `` placeholder wherever a value has no wire form;
  the REPL never asks, and asking costs nothing beyond what the surface rail
  already builds per command;
- `try` keeps only its flat error record;
- a `grant` contributes capability-check observations only when it sets
  `audit: true`; of those, a denied *head admission* also reaches the surface
  rail whether or not a trail is open, while an allowed check never does. An
  `fs` or full-argv denial reaches the trail alone — the command it refused
  still surfaces in its own right, as a failed command observation carrying
  the denial message;
- `audit` collects the full trail its body produces.

See also [[design/syscalls-are-effects|syscalls-are-effects]] — an audit trail
is a trace of the operations performed and the scopes that framed them.

Recording lives in `core/src/evaluator/audit.rs` ([[map/core/evaluator|evaluator]]);
the dispatch delimiter is the run door's own scope, held at `Shell::enter` in
`core/src/run.rs`, outside the `catch_unwind` a panicking run rolls back
through — a panic still drains and closes the scope, but reports `Static`
rather than carrying a trail; `docs/SPEC.md` §13.3 gives the formal
account.
