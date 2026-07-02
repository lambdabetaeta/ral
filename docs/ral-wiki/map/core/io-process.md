---
generated_at_commit: a007f72
generated_at_date: 2026-07-02
covers_paths: [core/src/io/, core/src/io.rs, core/src/process/, core/src/process.rs, core/src/stream.rs]
---

# Map: core / IO, process & stream

The byte plumbing under the [[map/core/evaluator|evaluator]]'s pipelines and
external commands: where a stage's bytes come from and go, the signals and
process groups that govern a foreground child, the daemon that fires every
scheduled action, and the labels the lazy Stream protocol shares with the type
system. **Authority over the controlling terminal is carried as a value, not
re-derived from process state** — the foreground handoff is gated on a held
[[map/core/shell-state|TerminalLease]].

## IO — `core/src/io/`

`io.rs` holds `Io`, the per-`Shell` bundle (stdin / stdout / stderr /
interactive / terminal / launch_role / capture_outer / capture_depth), and
*`LaunchRole`* — the process-group role distinguishing the top-level
orchestrator (`TopLevel`) from a pipeline-local child (`PipelineStage`). It
decides pgid *placement* (a top-level standalone external may lead its own
group so a watchdog cancel can `kill(-pgid, …)` the whole subtree; a stage
joins the pipeline's pgid) and forgives SIGPIPE on pipeline children — never
who may foreground. `Io::inherit_from` / `return_to` move the read-once stdin
between parent and child shells.

- `source.rs` — `Source`, a stage's byte input: `Pipe` (upstream stage),
  `File` (a `<file` redirect parked here), `Terminal` (fall through to fd 0),
  and `Empty` (no input — immediate EOF, child stdin to `/dev/null`, *no*
  fall-through to fd 0). `Empty` is what an exarch tool turn installs so a tool
  command can never steal the TUI's terminal; it is kept distinct from
  `Terminal` precisely so denial of byte input and denial of foreground stay
  separate effects.
- `sink.rs` — `Sink`, byte output and child stdio routing (`ChildStdioPlan`):
  terminal, stderr, kernel pipe, redirect file, in-memory `ByteBuffer` capture,
  tee, frontend printer, line-framing adapter. `child_stdout` / `child_stderr`
  centralise the (stdio, pump) decision so no caller computes inherit-vs-pipe by
  hand.
- `terminal.rs` — `TerminalState`: cached startup isatty / ANSI / NO_COLOR /
  mode bits. `startup_foreground` records whether ral's group owned the
  controlling terminal's foreground at entry; it is no longer a per-handoff
  oracle but the lease's *mint condition*
  ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).

Redirect reads and writes — `< file`, `> file` and friends — open through the
`File` source/sink here, and the runtime emits a byte-level I/O door at each:
the read fires eagerly when stdin is redirected, the write at frame settle with
its committed / aborted / failed outcome. The event shapes and their card
rendering belong to [[map/exarch/io-surface|io-surface]].

## Process — `core/src/process/`

- `outcome.rs` — `Signal`, `WaitOutcome`, and the user-facing `SpawnFailure` /
  `CommandFailure` the evaluator surfaces.
- `lease.rs` — `TerminalLease`, the unforgeable authority to hand the
  controlling terminal to a child via `tcsetpgrp`. No public constructor,
  neither `Clone` nor `Copy`: a host cannot forge or duplicate it. Minted at
  most once at session construction iff ral owned the foreground at startup
  (`None` on a backgrounded or tty-less launch, and always on platforms with no
  `tcsetpgrp`), then lent per turn as `&TerminalLease` to the one chokepoint
  that foregrounds — `ForegroundGuard::try_acquire`, which is *uncallable*
  without the borrow. The type lives at [[map/core/shell-state|shell-state]];
  the rationale at [[decisions/260619_terminal-lease|terminal-lease]].
- `reaper.rs` — one lazily started, process-global daemon (`ral-reaper`) owning
  a min-ordered heap of `(when, action)` entries, firing each at its `Instant`.
  *Deadlines are data*, not a thread per worker: `arm_lifetime` /
  `arm_callback` push an entry and return a `#[must_use]` `Deadline` guard —
  dropped, the entry disarms; `keep`-consumed, it fires regardless (the
  fire-and-forget death-clock of a detached worker that outlives its `spawn`).
  The fired action is `Cancel(scope) | Run(closure)`: `Cancel` cancels a
  `CancelScope` with `CancelCause::Deadline` (the death-clock and foreground
  wall); `Run` invokes an opaque host closure once, the shape a scheduled
  wakeup rides — exarch arms a `Run` that posts a prompt and wakes its idle
  loop, and a detached agent worker arms a `Run` that cancels its own token at
  its ceiling. The reaper stays ignorant of prompts, cron, and sessions;
  recurrence is not a reaper concept — a recurring producer re-arms from inside
  its own `Run`, fired outside the heap lock so it cannot deadlock
  ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives]]).
- `signal.rs` — the global termination flag (`check` / `clear` /
  `is_interrupted`) polled cooperatively in hot loops
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]);
  `interrupt` sets that flag to exactly 1 without escalating toward the
  third-signal `_exit` — the non-escalating unwind a raw-mode frontend drives on
  Esc ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]);
  the cause-bearing `CancelScope` tree (`DurableRoot` / `ForegroundScope`,
  `CancelCause`) for structured-concurrency cancellation, with the per-turn
  signal-reachable slots (`request_foreground_cancel` / `request_root_cancel`)
  that let a handler or TUI thread cancel a scope it cannot hold; `Pgid` /
  `PgidPolicy` / `ChildHandle` and the platform `spawn_with_pgid` family for
  process-group placement. Unix `ForegroundGuard` takes the `&TerminalLease`,
  performs the `tcsetpgrp` handoff, snapshots and restores tty foreground /
  termios, and blocks SIGTTOU for the parent-only restore window; unix
  `interrupt_foreground_child` re-sends raw-mode Esc/Ctrl-C to a foreground
  external group, `relay_handler` fans SIGINT to active external pgids, and
  `quit_handler` is the Ctrl-`\` root abort. Platform handlers live in
  `signal/unix.rs` and `signal/windows.rs`. Windows maps the pipeline pgid
  abstraction onto `CREATE_NEW_PROCESS_GROUP` plus a Job Object; descendants are
  covered once the child is assigned to the job, but a direct external that forks
  before post-spawn assignment can escape until launch moves to creation-time job
  placement. The whole stop-work flow — the `Interrupt < Explicit < Deadline <
  RootAbort` order — is narrated in
  [[internals/cancellation|cancellation]].

Spawning an external command is capability-gated; that gate lives in
[[map/core/capabilities|capabilities]], and the command/pipeline dispatch that
drives this plumbing in [[map/core/runtime|runtime]].

## Stream — `core/src/stream.rs`

Shared label vocabulary for the lazy Stream protocol: runtime variant labels
`more` / `done` and the `head` / `tail` payload fields, with the type-row
spellings (`` `more `` / `` `done ``) kept beside them so runtime and
[[map/core/typecheck|typechecker]] recognition cannot drift. `docs/SPEC.md` §13
covers Stream semantics.
