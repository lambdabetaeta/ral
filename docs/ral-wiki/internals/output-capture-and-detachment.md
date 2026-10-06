---
verified_at_commit: 1776d222
verified_at_date: 2026-09-30
anchors: [Sink::pump, SINK_BUFFER_CAP, WaitedChild, spawn_child, PgidPolicy::NewLeader, process::deadline, WorkerLease, WorkerRegistry, LeaseChain, spawn_detached, DetachPolicy, Capture, decode_utf8_strict, write_sink, CapturedBytes::overflowed]
---

# Output capture and detachment

**A run captures a child's output by draining its stdout/stderr pipe to
end-of-file, so a process that never closes that pipe is foreground work the run
must wait on to its wall — and `spawn` is the construct that moves such work off
the run onto a root-parented, byte-bounded, lease-bound worker.** A long-running
server is the canonical instance: run inline it stalls the call to the deadline
and is killed with its tree; spawned it returns instantly and survives, reaped
only for neglect — or never, if born a `service`.

## The capture frame: a computation's own bytes

**This section names a different mechanism from the drain-to-EOF story
below, though both swap in a `Sink::Buffer`.** The evaluator's `Capture` node
is written by the checker, and only by it: where a value is demanded of a
computation of type `F^w Unit` (a `let`'s right-hand side, an argument to a
value-demanding function, an arm joined with a value arm) it wraps the
computation in `cap M to d. decode d` (`let v = echo hi`; see
[[internals/type-inference|type-inference]] and
[[decisions/260930_graded-f|graded-f]]). A command's value is its output, so the
frame inspects no produced value: `capture : F^w Unit → F Bytes`.

- `CompKind::Capture` pushes `Frame::Capture` (`core/src/evaluator/machine.rs`,
  [[internals/evaluator-machine|evaluator-machine]]) and swaps `shell.io.stdout`
  to a fresh `Sink::Buffer` for the run of its operand, the frame holding the
  sink it replaced.
- On success, `Frame::Capture`'s return rule restores that sink and takes the
  buffer exactly, as `Value::Bytes`; the operand's own value is ignored. The
  text a value boundary reads comes from the `Decode` node the checker binds
  over it — no frame of its own, since the kernel's `decode` takes a value and
  `step_eval` reads it inline: one trailing terminator stripped, then
  `decode_utf8_strict`, the bind's scope dropped first so the buffer moves into
  the string rather than being copied. A decode failure names `| from-bytes` as
  the route for output that is not valid UTF-8.
- On a halt, `Frame::Capture`'s halt rule flushes whatever bytes the operand
  already wrote to the sink it replaced (`Shell::write_sink`), then propagates
  the halt. A failed operand's partial output is therefore not silently lost.
- An operand may contain an external child. That child's stdout still drains
  through the ordinary pump-to-EOF machinery below. It lands in the
  `Capture` buffer rather than in the terminal.

**A discarded statement writes to `stdout`, wherever that is.** A block is a
right-nested `Bind`, `a; b` being `a to _. b`
([[internals/evaluator-machine|evaluator-machine]]), and stepping it touches no
sink. Inside a capture `stdout` is the buffer, and the capture wraps the whole
right-hand side, so everything the block writes is the value unless an inner
`let` takes it: `let x = !{ echo a; echo b }` prints nothing and binds
`"a\nb"`, while `let x = !{ let y = echo a; echo b }` prints nothing and binds
`b`. A captured stand-in's every statement is captured.

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
  disarmable entry on the shared `process::deadline` scheduler (deadlines-as-data);
  on expiry the worker's child-wait loop fires `terminate_group`.
- A non-interactive exarch external leads its own process group
  (`PgidPolicy::NewLeader`, `core/src/runtime/command/foreground.rs`, gated by
  [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]),
  so the cancel SIGTERMs then unconditionally SIGKILLs the *whole group* —
  grandchildren included. Every copy of the write end closes, the pump sees EOF,
  the drain joins, and the call returns at the wall with exit 124. The server dies
  with it.
- The child's own report names the cause, not the signal: a death by the
  deadline's own teardown signal is attributed to `Deadline`
  ([[internals/cancellation|cancellation]]), so the command reads `timed out`
  at status 124 rather than `killed by signal 15`.
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
  (`core/src/builtins/concurrency/birth.rs`), drained into the handle's cache only when it
  settles. There is no run-owned pipe for it to hold open, so the run cannot
  stall on it.

## A chatty server is bounded, not unbounded

- Every `Sink::Buffer` is capped at 16 MiB (`SINK_BUFFER_CAP`). Past the cap
  `CapturedBytes::append` appends a one-line truncation marker and drops the rest — yet the
  write still returns `Ok` (`core/src/io/sink.rs`).
- So the pump keeps reading and discarding after the cap. A server that spews to
  stdout never fills the kernel pipe — it never blocks on a full pipe — and the
  worker's memory stays bounded at ~16 MiB. The detached path has no unbounded-growth
  failure mode, and no undrained-pipe stall.
- Truncation with a marker is *this* path's contract, and only this one. A
  worker's bytes arrive with nobody to refuse them: the pump is a thread whose
  failure has no one to raise it to, and the buffer has to stay bounded whether
  or not the handle is ever awaited — so the report travels in band, and
  `await` hands on a prefix that says where it stopped. Where the bytes *are*
  the value — `capture` — the buffer is drained by the very step that would
  bind them, so `Frame::Capture`'s return rule reads `CapturedBytes::overflowed` once
  the writers have joined and fails, flushing the prefix visibly rather than binding it
  ([[design/capture|capture]]). One flag on the buffer, two readings of it.

## Detachment decays by neglect, not by age

- Every detached worker — `spawn`, `watch`, `service` — files a `WorkerEntry` in
  its shell's `WorkerRegistry` the instant it starts (`core/src/types/shell/workers.rs`):
  a per-shell directory holding the handle itself, not a second by-id control
  plane. All three classes are meant to end with the host process, and `detach`
  files nothing here because it is not a worker at all — it is meant to outlive
  the host ([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).
  `poll`, `await`, `race`, and `cancel` stay the only verbs that touch a
  worker, and there is no model-facing listing over the registry at all — the
  `workers` builtin was retired, since a listing carrying live `Value::Handle`s
  can never cross the engine protocol. Rediscovery instead splits by class: an
  ordinary `spawn`/`watch` worker (`LeaseClass::Worker`) is rediscovered
  through the binding lease, never by id; a `service`-born worker
  (`LeaseClass::Durable`) is rediscovered by the id its birth trail card
  named (`worker #id cmd durable`) — the pin register once carried a
  host-owned `services` listing for this, deleted with the rest of the
  protected-pin mechanism
  ([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]'s
  2026-08-27 amendment) — and
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
- The mechanism is the deadline scheduler's own re-arming `Run` entry
  (`process::arm_callback`, `LeaseChain::fire` in `core/src/types/shell/workers.rs`):
  each firing asks `WorkerLease::verdict` — a pure function of the worker's age
  and of the idle time off the handle's shared last-observed cell: the backstop
  first, then the idle bound — and either reaps or re-arms itself for the sooner
  of the two remaining margins. A worker that has already settled (not
  `Running`) ends the chain silently — it lingers in the registry as an
  unclaimed result under its own, separate retention lease (256 idle ral
  calls, `SETTLED_WORKER_RETENTION`), swept by `WorkerRegistry::sweep_retention`
  on the host's ral-call epoch.
- `service <desc> { … }` births a worker whose registry entry carries the
  durable class (`LeaseClass::Durable`): no idle bound, no backstop ever arms
  for it — legibility is the whole bound, structural now that `desc` is a
  mandatory single-line description: the host's `services` pin lists it by
  id and description, `service-handle <id>` retakes its handle, and it dies by
  `/clear`, an explicit `cancel`, or — by intent rather than by construction,
  since teardown's cancel flag races the exiting main thread — with the host
  process.
- Every reap — idle, backstop, or settled-retention — is atomic with a
  `ReapNotice` recording what fell and why. The engine drains its ledger at
  each run's own ready boundary (`Shell::emit_ready_boundary_notices`, called
  from `run_framed` just before the frame tears down) and pushes it through
  that run's surface sink as a `` `notice `` value, beside the binding-lease's
  idle-prune notice; exarch decodes it back (`card::value_to_notice`) into a
  `Forensic::Reap` record that no printer draws. The model's later "where did my job go?" always has an
  answer in the log.

## Reading a spawned server's output (the exarch caveat)

- A server never settles, so `await $h` would block to the wall and unwind —
  sparing the root-parented worker, via the cancel-aware `wait_first_settled`. But
  `poll $h` is a pull-based read of a *running* worker: its `` `pending `` arm
  carries a `{stdout, stderr}` snapshot of the bytes buffered so far, cloned
  non-destructively (`CapturedBytes::peek`, not the completion `take`), so the buffer
  is left intact and a later `await`/`` `settled `` `poll` still sees everything
  ([[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  The snapshot is cumulative — each poll of a live worker reports monotonically more
  — and it is capped by `SINK_BUFFER_CAP` like every capture buffer.
- `watch` — the one primitive that *streams* a running worker's output live —
  rides the session surface: each line leaves as one `` `watch [label, line] ``
  value through the session-lived deferred sink (`Sink::Watch`), a batch of one,
  which the REPL host prints through rustyline's external printer above the
  prompt and batch prints to stdout. Only the ral hosts install it
  ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]); partial `poll`
  is exarch's substitute: not a live stream, but a poll-driven read exarch can
  drive from its own runs.
- So under exarch a server is *fire-and-`poll`-and-`cancel`*: `spawn` it, read its
  accumulated output with `poll $h` on later runs, `cancel $h` when done. `poll`
  is also what keeps a plain `spawn`ed server alive past an hour of inattention
  — it renews the idle-observation lease. A server known at birth to go long
  stretches unpolled wants `service <desc> { … }` instead: born with no idle bound and
  no backstop, it never reaps for inattention, only by `/clear`, `cancel`, or
  the host's own exit — and a server that must still be answering *after* that
  exit is not a worker at all, but a `detach` (below). To keep a full,
  unbounded log past the 16 MiB cap, still
  redirect inside the block to a file — `spawn { python3 -m http.server >
  srv.log 2>&1 }` — and read the file on later runs.

## `detach` leaves the machine, not just the run

**Every mechanism above is in-process: the pump, the buffer cap, the lease, the
registry all presuppose a thread the session can still reach.** `detach` gives
that up — the thing to escape is not the session but the parent's *observation*
of the child's pgid.

- Everything up to the birth is the ordinary external-command machinery —
  identity, `vet`, `build_launch` (`core/src/runtime/command/detach.rs`), so the
  grant judges the call exactly as it judges any exec and a head a handler in
  scope intercepts is refused, birthing nothing: a handler runs inside this
  session, so there is nothing to detach (to run the real program, `^name`). Only the last
  act differs: `Launch::spawn_detached` **double-forks**. The intermediate exits at
  once and hands the grandchild's pid back over a pipe, so no `RunningChild` ever
  records the pgid both kill paths address. Reaching the same lifetime by hiding
  a worker outside the session's `DurableRoot` cancel scope was rejected: it
  defeats the signal but leaves the child holding a pipe that closes at exit,
  killing anything that logs.
- The grandchild severs the rest **itself**: `setsid()` in its own body, since a
  bare double fork leaves its pgid naming the intermediate's recycled pid; and
  fd 0, fd 1, fd 2 all on `/dev/null`. There is no pump, so none of the
  drain-to-EOF story above applies, and no `SINK_BUFFER_CAP` — there is nothing
  to buffer.
- What comes back is a **receipt** — `{ pid, desc }`, an `F [pid: Int, desc: String]`
  record rather than a `Value::Handle` — and nothing is written anywhere. So there is nothing for a
  `LeaseClass` to grade, nothing for `cancel_all` to reach, and no exit status:
  the session never waited and cannot. The survivor is *mute*: a program worth
  outliving a session keeps its own log, and the only way to learn whether it is
  still alive is to probe what it serves.
- The verb is **absent, not vetoed**, wherever it has no meaning. A host arms a
  `DetachPolicy` — a birth budget — in the same act that installs the builtin,
  and does so only off Windows, where the double fork exists. That is now the
  whole of the absence question: whether a given *call* may spend the verb is
  asked of the live grant stack (`GrantStack::permits(Flag::Detach)`) and answered as
  a refusal, `detach: false`
  ([[decisions/260727_detach-under-a-grant|detach-under-a-grant]]).
- A survivor born under a projection **keeps it for life**. `build_launch`
  renders the frame's confinement into the launch exactly as for a child the
  session keeps; `Ownership::Surrendered` drops only the two ties between the
  session and the envelope — death (`--die-with-parent`, which against a double
  fork would kill the survivor moments after birth or never fire at all) and
  address (`--info-fd`, there being no session left to address it). The
  namespaces stay: the envelope's init outlives the session, so the daemon
  runs confined until it exits, its `getpid()` namespace-local and its process
  invisible from inside any later grant. Nothing later can widen what it may
  touch, because nothing later can name it
  ([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).
- A detached row has no place in a resident fold even if one wanted the
  ledger's uniformity, because that fold demands `cancel()` and every answer is
  wrong — a no-op lies about the edge, a `kill` re-asserts the ownership the
  verb exists to renounce ([[design/residency|residency]]).

See also
[[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]
(the verb, the race it replaces, and why the type change earns the name),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(why a handle marks detachment, and the doctrine
leases-and-budgets retired: not
"detached workers are unmanaged by design" but unmanaged by default),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the root/foreground
split and the reaper), [[map/exarch/shell-eval|shell-eval]] (the frame that arms the
wall and captures the bytes), [[internals/binding-leases|binding-leases]] (the
lease idiom applied to scratch names, the same page's sibling story),
[[map/core/io-process|io-process]], and `docs/SPEC.md` §11.2, §11.5.
