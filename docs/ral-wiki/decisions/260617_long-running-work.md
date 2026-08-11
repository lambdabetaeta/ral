---
status: superseded
---

# Long-running work is born, not promoted

> Superseded by leases-and-budgets: the
> open regime question is answered as Regime 1, delivered as the durable lease
> class of a universal worker registry; the verb is spelled `service`, now
> with a mandatory `description`. The binding-pinning question dissolves
> because the registry retains the handle itself. The cancel-by-id question
> is answered narrowly, not by a general listing: amended, 2026-07-06, the
> registry grew a model-facing `workers` listing and then retired it again —
> a never-bound durable job is instead re-acquired by `service-handle <id>`
> off a host-owned pinned ledger row, never a queried value.

**Work that must outlive the one-hour detached-worker death-clock is *born*
durable — launched as its own construct with its own lifetime — not started as
an ordinary `spawn` and promoted later.** This is the second, explicit mechanism
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
deferred. Its shape is settled — a distinct exarch-registered verb that births a
worker into a listable durable-job registry — but its *headline* decision is
open: which lifetime **regime(s)** it supports, and so which to build.

## Context

Two bounds exist; long work collides with only one.

- **The 30 s foreground wall** (`CALL_TIMEOUT_SECS`, `exarch/src/tools/ral.rs:31`)
  bounds a *synchronous* tool call. Long work already escapes it via the canonical
  idiom — `spawn` → `poll` across turns → `await` once settled — so this is not
  the bound that pinches.
- **The one-hour death-clock** reaps an *abandoned* detached `spawn` worker. That
  is the bound a long job collides with.

The death-clock is realised by arming the shared reaper. `spawn_child` does
`crate::process::arm_lifetime(cancel.clone(), ceiling).keep()`
(`core/src/builtins/concurrency.rs`), where `ceiling` is the frame's
`detached_ceiling: Option<Duration>` (`core/src/types/shell/mod.rs`). exarch
supplies `Some(DETACHED_WORKER_CEILING)` = one hour
(`exarch/src/shell_eval.rs`); the REPL supplies `None`. `.keep()` is
fire-and-forget: the reaper's `Deadline` guard
(`core/src/process/reaper.rs`) is forgotten, so the ceiling fires even when
the handle is dropped — that is precisely what reaps an *abandoned* worker.
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]] collapsed the frame's
detached-worker policy to this bare ceiling when `watch` admission left the
frame.)

The concurrency ADR named this follow-up directly: long-running work should be
"a second, explicit mechanism … a host-managed durable job, with its own
ownership, listing, cancellation, and lifetime policy," and it **rejected a
`timeout:` knob on ordinary `spawn`**. This ADR is that mechanism's design.

## Decided

- **Birth, not promote.** A long job is *born* durable, not started as an
  ordinary `spawn` and promoted afterwards. The mechanical reason: a born-durable
  worker never arms the death-clock, so there is nothing to undo. Promotion, by
  contrast, would have to retain the death-clock's `Deadline` guard in a side
  registry keyed by id — solely so a later call could *disarm* it — machinery
  whose only purpose is to undo a ceiling a promoted worker wishes had never been
  armed. The intent reason: a server or a multi-hour run is known to be long *at
  launch*; the "spawn, then realise it must persist" case promotion serves is
  rare. So build birth only; add a `promote` operation later only if a concrete
  mid-flight need appears — not speculatively.

- **A distinct verb, registered by exarch.** Birth is its own construct, not a
  knob on `spawn` (the concurrency ADR rejected the knob, and `spawn` stays one
  boring primitive). It is an *exarch-registered builtin*, reusing the principle
  from [[decisions/260617_watch-repl-builtin|watch-repl-builtin]]: **core supplies
  the spawn mechanism, the host registers the affordance.** The REPL does not get
  it — the REPL has its own POSIX job control (`jobs`/`fg`/`bg`/`disown`,
  `ral/src/jobs.rs`, [[map/repl/jobs|repl/jobs]]); durable agent jobs are exarch's
  concern. Because `disown` already names REPL job control, it is unavailable as a
  spelling here.

- **Born into a durable-job registry → listable.** This is the management surface
  the concurrency ADR reserved. It is load-bearing: across context compaction the
  model loses the handle's *binding name*, so it must rediscover its jobs by
  *listing* and then `poll`/`cancel` by id rather than by name. A born-durable
  handle must therefore be *pinned* against
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]], so its
  binding is not pruned out from under it.

## The open decision: which regime(s)?

Birth means two very different things by lifetime. They share almost no code, so
the real decision is which to build.

- **Regime 1 — in-process durable** (dies when exarch exits). The worker is an
  in-process OS thread that never arms the one-hour ceiling (or arms a long
  backstop instead). It returns a real `Handle`, so `poll`/`await`/`cancel` all
  still apply; it is registered and listable. It reuses the existing
  reaper/handle/registry machinery, so the implementation is small. It fits "a
  multi-hour build or training run I babysit across this session." Candidate
  spelling: `service { … }` (neutral) or `daemon { … }` (evocative, but `daemon`
  connotes *survives process exit*, which this regime does not).

- **Regime 2 — survives exarch exit** (a server you leave running). This is *not
  a ral-handle concept at all*: an in-process thread cannot outlive its process.
  It is launching a detached OS process that escapes the worker's process-group
  teardown — a new session via `setsid`, IO redirected to files — reconnected
  later by pid and exit status. There is no `Handle`, no `poll`/`await`; it is
  closer to `nohup`/process-group detachment than to a ral job. Observe that
  Regime 2 independently confirms *birth, not promote*: you cannot promote an
  in-process thread into a surviving process.

**Recommendation.** Build **Regime 1** now — the in-process durable birth
primitive, with listing and cancel-by-id. Treat **Regime 2** as a separate,
later, smaller process-detachment capability, pursued only if a concrete need
appears. Do not fuse the two: they share almost no implementation.

## Alternatives considered

- **Promote an existing `spawn` handle.** Rejected for birth: the guard-retention
  machinery above, and intent is known at launch. A `promote` verb may return
  later for a concrete mid-flight need; it is not built speculatively.
- **A lifetime / `timeout:` knob on ordinary `spawn`.** Already rejected by the
  concurrency ADR. `spawn` stays one boring primitive with a host-owned default
  ceiling; long life is a different construct, not a dial on every spawn.
- **Fuse both regimes into one primitive.** Rejected: different handle types
  (`Handle` vs none), different observability (`poll`/`await` vs pid/status),
  different lifetime, and essentially no shared code. One primitive spanning both
  would be two implementations behind one name.

## Open questions

- **Regime 1 only, or also Regime 2?** The headline decision this page frames.
- **Spelling:** `service` vs `daemon` (the latter mis-suggests survives-exit).
- **Regime-1 lifetime:** truly no ceiling (a forgotten job is a zombie until
  exarch exits) versus a long backstop (e.g. 24 h) with the listing surface to
  bound it by hand.
- **The cancel-by-id surface** for a job whose handle *binding* is gone — the
  registry must answer cancellation by id, not by the lost name.

## See also

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the ADR that deferred this; the death-clock and `timeout:`-knob rejection),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the frame and
the root/foreground split the death-clock rides),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the core-mechanism /
host-affordance registration principle this reuses),
[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] (a born-durable
handle must be pinned against the reaper),
[[map/repl/jobs|repl/jobs]] (the REPL's POSIX job control this is distinct from),
[[map/core/builtins|map: builtins]], and `docs/SPEC.md` §11.
