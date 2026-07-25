---
verified_at_commit: f7cf93a
verified_at_date: 2026-07-25
anchors: [Sink::pump, SINK_BUFFER_CAP, WaitedChild, spawn_child, PgidPolicy::NewLeader, process::reaper, WorkerLease, WorkerRegistry, lease_fire]
---

# Output capture and detachment

**A run captures a child's output by draining its stdout/stderr pipe to
end-of-file, so a process that never closes that pipe is foreground work the run
must wait on to its wall — and `spawn` is the construct that moves such work off
the run onto a root-parented, byte-bounded, lease-bound worker.** A long-running
server is the canonical instance: run inline it stalls the call to the deadline
and is killed with its tree; spawned it returns instantly and survives, reaped
only for neglect — or never, if born a `service`.

## Capture is a drain to EOF

A captured stream is a `Sink::Buffer` fed by a *pump* — see [[map/core/io-process|io-process]].

- `Sink::pump` spawns a thread running `io::copy(child_pipe, sink)` until the pipe
  reaches end-of-file (`core/src/io/sink.rs`).
- The pump is joined *after* the child is waited: `WaitedChild` joins the
  pump handles, and the typestate makes draining-before-waiting unwritable
  (`core/src/runtime/command/child.rs`, [[map/core/runtime|runtime]]). A foreground
  command returns only once every byte the child wrote has been copied and the
  pipe has closed.
- The release condition is *EOF, not exit*. A pipe closes when its last writer's
  descriptor closes — so a child that has itself exited but left a grandchild
  holding the inherited write end keeps the pump blocked.

## A never-closing pipe stalls the foreground to the wall

- A server holds its stdout open for its whole life, so the pump's `io::copy`
  never sees EOF and the foreground command blocks indefinitely.
- The release is the *foreground deadline*. exarch arms a 30 s wall as a
  disarmable entry on the shared `process::reaper` (deadlines-as-data); on expiry
  the worker's child-wait loop fires `terminate_group`.
- A non-interactive exarch external leads its own process group
  (`PgidPolicy::NewLeader`, `core/src/runtime/command/foreground.rs`, gated by
  [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]),
  so the cancel SIGTERMs then unconditionally SIGKILLs the *whole group* —
  grandchildren included. Every copy of the write end closes, the pump sees EOF,
  the drain joins, and the call returns at the wall with exit 124. The server dies
  with it.
- This is correct, not a defect: an inline command that never closes its pipe is
  genuinely work the run cannot finish, so the run bounds it and tears down its
  tree. The cancel→drain→collect path is the same one pipelines reap by
  ([[internals/pipeline-execution|pipeline-execution]]).

## `spawn` moves the work off the run

The escape is detachment — the *handle* is its evidence
([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).

- `spawn { … }` reifies a `Value::Handle` and runs the body on a worker thread
  parented at the *durable root*, not the swappable foreground scope
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]). The 30 s wall
  never reaches it, so the worker survives the run.
- `spawn` returns the handle the instant the thread starts; the run does not
  wait. A server spawned this way keeps running while the launching run returns in
  milliseconds.
- The worker's output goes to its *own* per-handle buffer — `spawn_child` wires the
  child's `stdout`/`stderr` to fresh `new_buffer()` sinks
  (`core/src/builtins/concurrency.rs`), drained into the handle's cache only when it
  settles. There is no run-owned pipe for it to hold open, so the run cannot
  stall on it.

## A chatty server is bounded, not unbounded

- Every `Sink::Buffer` is capped at 16 MiB (`SINK_BUFFER_CAP`). Past the cap
  `write_capped` appends a one-line truncation marker and drops the rest — yet the
  write still returns `Ok` (`core/src/io/sink.rs`).
- So the pump keeps reading and discarding after the cap. A server that spews to
  stdout never fills the kernel pipe — it never blocks on a full pipe — and the
  worker's memory stays bounded at ~16 MiB. The detached path has no unbounded-growth
  failure mode, and no undrained-pipe stall.

## Detachment decays by neglect, not by age

- Every detached worker — `spawn`, `watch`, `service` — files a `WorkerEntry` in
  its shell's `WorkerRegistry` the instant it starts (`core/src/types/shell/workers.rs`):
  a per-shell directory holding the handle itself, not a second by-id control
  plane. `poll`, `await`, `race`, and `cancel` stay the only verbs that touch a
  worker, and there is no model-facing listing over the registry at all — the
  `workers` builtin was retired, since a listing carrying live `Value::Handle`s
  can never cross the host seam. Rediscovery instead splits by class: an
  ordinary `spawn`/`watch` worker (`LeaseClass::Worker`) is rediscovered
  through the binding lease, never by id; a `service`-born worker
  (`LeaseClass::Durable`) is rediscovered through the host-owned `services`
  pin, which shows each live service's id and birth description, and
  `service-handle <id>` (`exarch/src/shell_eval/builtins.rs`) takes the handle back
  by that id to resume the ordinary eliminator idiom
  ([[map/exarch/builtins|builtins]]).
- An ordinary `spawn`/`watch` worker (`LeaseClass::Worker`) is governed by the
  frame's `WorkerLease`: an idle bound on the *observation* clock, under an
  absolute backstop. It is reaped once unobserved — no `poll`/`await`/`race`
  has named its handle — for `idle`, or once older than `backstop` regardless
  of observation. exarch grants one hour idle / 24 hour backstop
  (`DETACHED_WORKER_CEILING`, `DETACHED_WORKER_BACKSTOP`,
  `exarch/src/shell_eval.rs`); the REPL grants none, so its spawns never reap.
  Age alone no longer kills a worker: a build babysat every run via `poll`
  renews indefinitely, up to the backstop.
- The mechanism is the reaper's own re-arming `Run` deadline
  (`process::arm_callback`, `lease_fire` in `core/src/builtins/concurrency.rs`):
  each firing checks the backstop first, then the idle bound off the handle's
  shared last-observed cell, and either reaps or re-arms itself for the sooner
  of the two remaining margins. A worker that has already settled (not
  `Running`) ends the chain silently — it lingers in the registry as an
  unclaimed result under its own, separate retention lease (256 idle ral
  calls, `SETTLED_WORKER_RETENTION`), swept by `WorkerRegistry::sweep_retention`
  on the host's ral-call epoch.
- `service <desc> { … }` births a worker whose registry entry carries the
  durable class (`LeaseClass::Durable`): no idle bound, no backstop ever arms
  for it — legibility is the whole bound, structural now that `desc` is a
  mandatory single-line description: the host's `services` pin lists it by
  id and description, `service-handle <id>` retakes its handle, and it dies
  only by `/clear`, an explicit `cancel`, or process exit.
- Every reap — idle, backstop, or settled-retention — is atomic with a
  `ReapNotice` recording what fell and why. The engine drains its ledger at
  each run's own ready boundary (`Shell::emit_ready_boundary_notices`, called
  from `run_framed` just before the frame tears down) and pushes it through
  that run's surface sink as a `` `notice `` value, beside the binding-lease's
  idle-prune notice; exarch decodes it back (`card::value_to_notice`) into a
  `Kind::Notice` transcript/TUI event. The model's later "where did my job
  go?" always has an answer in the log.

## Reading a spawned server's output (the exarch caveat)

- A server never settles, so `await $h` would block to the wall and unwind —
  sparing the root-parented worker, via the cancel-aware `wait_first_settled`. But
  `poll $h` is a pull-based read of a *running* worker: its `` `pending `` arm
  carries a `{stdout, stderr}` snapshot of the bytes buffered so far, cloned
  non-destructively (`peek_buffer`, not the completion `take_buffer`), so the buffer
  is left intact and a later `await`/`` `settled `` `poll` still sees everything
  ([[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  The snapshot is cumulative — each poll of a live worker reports monotonically more
  — and it is capped by `SINK_BUFFER_CAP` like every capture buffer.
- `watch` — the one primitive that *streams* a running worker's output live — is
  still REPL-only: exarch's per-call capture sinks cannot host a root-surviving
  writer ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]). Partial `poll`
  is the headless substitute: not a live stream, but a poll-driven read exarch can
  drive from its own runs.
- So under exarch a server is *fire-and-`poll`-and-`cancel`*: `spawn` it, read its
  accumulated output with `poll $h` on later runs, `cancel $h` when done. `poll`
  is also what keeps a plain `spawn`ed server alive past an hour of inattention
  — it renews the idle-observation lease. A server known at birth to go long
  stretches unpolled wants `service <desc> { … }` instead: born with no idle bound and
  no backstop, it never reaps for inattention, only by `/clear`, `cancel`, or
  process exit. To keep a full, unbounded log past the 16 MiB cap, still
  redirect inside the block to a file — `spawn { python3 -m http.server >
  srv.log 2>&1 }` — and read the file on later runs.

See also
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(why a handle marks detachment, and the doctrine
leases-and-budgets retired: not
"detached workers are unmanaged by design" but unmanaged by default),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the root/foreground
split and the reaper), [[map/exarch/shell-eval|shell-eval]] (the frame that arms the
wall and captures the bytes), [[internals/binding-leases|binding-leases]] (the
lease idiom applied to scratch names, the same page's sibling story),
[[map/core/io-process|io-process]], and `docs/SPEC.md` §13.3.
