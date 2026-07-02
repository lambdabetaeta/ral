---
verified_at_commit: a590f4f
verified_at_date: 2026-06-18
anchors: [Sink::pump, SINK_BUFFER_CAP, WaitedChild::drain, spawn_child, PgidPolicy::NewLeader, process::reaper, detached_ceiling]
---

# Output capture and detachment

**A turn captures a child's output by draining its stdout/stderr pipe to
end-of-file, so a process that never closes that pipe is foreground work the turn
must wait on to its wall — and `spawn` is the construct that moves such work off
the turn onto a root-parented, byte-bounded, time-bounded worker.** A long-running
server is the canonical instance: run inline it stalls the call to the deadline
and is killed with its tree; spawned it returns instantly and survives, reaped
only by its ceiling.

## Capture is a drain to EOF

A captured stream is a `Sink::Buffer` fed by a *pump* — see [[map/core/io-process|io-process]].

- `Sink::pump` spawns a thread running `io::copy(child_pipe, sink)` until the pipe
  reaches end-of-file (`core/src/io/sink.rs`).
- The pump is joined *after* the child is waited: `WaitedChild::drain` joins the
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
  genuinely work the turn cannot finish, so the turn bounds it and tears down its
  tree. The cancel→drain→collect path is the same one pipelines reap by
  ([[internals/pipeline-execution|pipeline-execution]]).

## `spawn` moves the work off the turn

The escape is detachment — the *handle* is its evidence
([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).

- `spawn { … }` reifies a `Value::Handle` and runs the body on a worker thread
  parented at the *durable root*, not the swappable foreground scope
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]). The 30 s wall
  never reaches it, so the worker survives the turn.
- `spawn` returns the handle the instant the thread starts; the turn does not
  wait. A server spawned this way keeps running while the launching turn returns in
  milliseconds.
- The worker's output goes to its *own* per-handle buffer — `spawn_child` wires the
  child's `stdout`/`stderr` to fresh `new_buffer()` sinks
  (`core/src/builtins/concurrency.rs`), drained into the handle's cache only when it
  settles. There is no turn-owned pipe for it to hold open, so the turn cannot
  stall on it.

## A chatty server is bounded, not unbounded

- Every `Sink::Buffer` is capped at 16 MiB (`SINK_BUFFER_CAP`). Past the cap
  `write_capped` appends a one-line truncation marker and drops the rest — yet the
  write still returns `Ok` (`core/src/io/sink.rs`).
- So the pump keeps reading and discarding after the cap. A server that spews to
  stdout never fills the kernel pipe — it never blocks on a full pipe — and the
  worker's memory stays bounded at ~16 MiB. The detached path has no unbounded-growth
  failure mode, and no undrained-pipe stall.

## Detachment is bounded in time too

- An abandoned exarch worker is reaped by the *death-clock*: the frame arms a
  one-hour lifetime ceiling on each `spawn` worker's own scope as a kept entry on
  the same `process::reaper`. At expiry the scope flips `Deadline` and the worker's
  child-wait loop SIGKILLs the server's group exactly as the foreground wall does.
  A spawned server is therefore never immortal — reaped at one hour, or sooner by
  `cancel $h`.
- The lifetime is a frame policy (`detached_ceiling: Option<Duration>`), not a
  per-spawn knob: exarch arms one hour, the REPL arms none.

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
  drive from its own turns.
- So under exarch a server is *fire-and-`poll`-and-`cancel`*: `spawn` it, read its
  accumulated output with `poll $h` on later turns, `cancel $h` when done. To keep a
  full, unbounded log past the 16 MiB cap, still redirect inside the block to a file
  — `spawn { python3 -m http.server > srv.log 2>&1 }` — and read the file on later
  turns.

See also
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(why a handle marks detachment and where the death-clock lives),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the root/foreground
split and the reaper), [[map/exarch/shell-eval|shell-eval]] (the frame that arms the
wall and captures the bytes), [[map/core/io-process|io-process]], and
`docs/SPEC.md` §13.3.
