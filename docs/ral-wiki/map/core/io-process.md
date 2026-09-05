---
generated_at_commit: 4957c6c4
generated_at_date: 2026-09-04
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
interactive / terminal / launch_role / capture_outer), and
*`LaunchRole`* — the process-group role distinguishing the top-level
orchestrator (`TopLevel`) from a stage's own children (`PipelineStage(Pgid)`,
carrying the group an external spawned anywhere inside that stage must join —
at the stage's own root or nested arbitrarily deep, thread or process alike).
It decides pgid *placement* (a top-level standalone external may lead its own
group so a watchdog cancel can `kill(-pgid, …)` the whole subtree; anything
inside a stage joins that stage's pgid) and says whether a child's reader is
the caller or the next stage — never who may foreground. `pipeline/launch.rs`'s
`stage_stdin` resolves a stage thread's stdin against its route — the upstream
edge, or (`stage_stdin_parent`) a duplicate of the parent's own `shell.io.stdin`
— never taking or moving it, since `Source::reader` only ever hands out a
duplicate ([[internals/pipeline-execution|pipeline execution]]).

- `source.rs` — `Source`, a stage's byte input: `Terminal` (fall through to
  fd 0), `Empty` (no input — immediate EOF, child stdin to `/dev/null`, *no*
  fall-through to fd 0), and `Reader(SourceReader)` — a pipe (an upstream
  stage, a here-string) or a file (a `<file` redirect), and for a stage
  thread's own stdin the `Wake` a blocked read polls beside its fd; a fired
  wake reads as EOF, never as an interrupted-read error a retry loop would
  spin on. `Source::reader` never *takes* a source: it hands every consumer —
  a builtin, an external's stdin, a nested stage — a duplicate, the wake left
  behind when a child inherits it, so a stage's
  stdin outlives every command that reads it. `Empty` is also what a
  tty-owning pipeline's stage 0 resolves to when it has no upstream reader —
  a thread in the shell process cannot read a terminal whose foreground
  belongs to the externals' group — and what an exarch tool run installs so a
  tool command can never steal the TUI's terminal; it is kept distinct from
  `Terminal` precisely so denial of byte input and denial of foreground stay
  separate effects.
- `sink.rs` — `Sink`, byte output and child stdio routing (`ChildStdioPlan`):
  terminal, stderr, redirect file, in-memory `ByteBuffer` capture, tee,
  frontend printer, line-framing adapter. `child_stdout` / `child_stderr`
  centralise the (stdio, pump) decision so no caller computes inherit-vs-pipe by
  hand. A `ByteBuffer` is `Arc<CapturedBytes>`: the bytes under a mutex, and
  beside them the `overflowed` flag `write_capped` raises at
  `SINK_BUFFER_CAP`. The flag exists because the write path cannot report the
  cap — a pump returns `()` from its own thread — so `buffer_overflowed` is
  read once the writers have joined, by whoever means to make the bytes a
  value ([[design/capture|capture]]).
- `terminal.rs` — `TerminalState`: cached startup isatty / ANSI / NO_COLOR /
  mode bits. `startup_foreground` records whether ral's group owned the
  controlling terminal's foreground at entry; it is no longer a per-handoff
  oracle but the lease's *mint condition*
  ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
  On Windows it also owns `console_mode_snapshot` / `restore_console_mode`,
  the termios-snapshot analogue a panic hook restores a raw-mode console
  through.

Redirect reads and writes — `< file`, `> file` and friends — open through the
`File` source/sink here, and the runtime emits a byte-level I/O door at each:
the read fires eagerly when stdin is redirected, the write at frame settle with
its committed / aborted / failed outcome. The event shapes and their card
rendering belong to [[map/exarch/io-surface|io-surface]].

## Process — `core/src/process/`

- `outcome.rs` — `Signal`, `WaitOutcome`, and the user-facing `SpawnFailure` /
  `CommandFailure` the evaluator surfaces. A death ral itself caused is its
  own `WaitOutcome` variant (`Cancelled`, carrying the `CancelCause` and the
  signal we sent), so a torn-down child reports the cause — an expired time
  limit, a `cancel`, an interrupt, a shutdown — while a signal from outside ral
  still reports its number. `CommandFailure::from_outcome` takes `sent: Option<
  CancelCause>`, the strongest cause anything sent the child, joined by `max`
  where two parties each ended it — a cancellation in force outranks the
  collector's reader-gone kill (raised only once the sentinel hears a dead
  write) by that order rather than by a special case. It is the sole input to
  forgiveness for an external: only `Some(ReaderGone)`, and only a death that
  kill actually caused, keeps no failure; every other status is kept, because
  the kill precedes the wait and cannot rewrite a recorded status
  ([[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]],
  [[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]]).
- `wake.rs` — `Wake`, what ends a stage thread's blocked stdin read or
  stdout write from another thread: a self-pipe polled beside the stage's own
  fd on Unix, a flag plus `CancelSynchronousIo` on the stage's thread handle
  on Windows. `fire`/`is_fired` are the reader-visible surface; a fired wake
  is read as EOF by a `SourceReader` carrying it and written as success by a
  `Sink::Pipe` carrying it, never as an interrupted I/O error.
- `spawn_lock.rs` (`target_vendor = "apple"`) — the process-wide `RwLock`
  closing Apple's fork/`CLOEXEC` race: neither `pipe2` nor `SOCK_CLOEXEC` is
  atomic there, so a `fork` racing a pipe's create-then-`fcntl` window can hand
  an exec'd child a fd it should never have inherited. `cloexec_pipe` /
  `cloexec_socketpair` take the shared side; `spawn(cmd)` is the *only* fork
  door, holding the exclusive side around `Command::spawn` and nothing else —
  a `.output()`/`.status()` caller must spawn through it and wait outside the
  lock, or a child's whole lifetime would block every other thread's fd
  creation. `clippy.toml`'s `disallowed-methods` enforces both doors on this
  target. Every other target is the identity: no lock, no cost.
- `lease.rs` — `TerminalLease`, the unforgeable authority to hand the
  controlling terminal to a child via `tcsetpgrp`. No public constructor,
  neither `Clone` nor `Copy`: a host cannot forge or duplicate it. Minted at
  most once at session construction iff ral owned the foreground at startup
  (`None` on a backgrounded or tty-less launch, and always on platforms with no
  `tcsetpgrp`), then lent per run as `&TerminalLease` to the one chokepoint
  that foregrounds — `ForegroundGuard::try_acquire`, which is *uninvocable*
  without the borrow. The type lives at [[map/core/shell-state|shell-state]];
  the rationale at [[decisions/260619_terminal-lease|terminal-lease]].
- `deadline.rs` — one lazily started, process-global daemon (`ral-deadline`)
  owning a min-ordered heap of `(when, action)` entries, firing each at its
  `Instant`. *Deadlines are data*, not a thread per worker: `arm_lifetime` /
  `arm_callback` push an entry and return a `#[must_use]` `Deadline` guard —
  dropped, the entry disarms; `keep`-consumed, it fires regardless (the
  fire-and-forget mode a detached worker's lease needs, since the worker
  outlives the `spawn` that armed it).
  The fired action is `Cancel(scope) | Run(closure)`: `Cancel` cancels a
  `CancelScope` with `CancelCause::Deadline` (the foreground wall); `Run`
  invokes an opaque host closure once, the shape a scheduled
  wakeup rides — exarch arms a `Run` that posts a prompt and wakes its idle
  loop, a detached agent worker arms a `Run` that cancels its own token at
  its ceiling, and a detached `spawn` worker's idle-observation lease chain
  ([[map/core/builtins|builtins]]) is a `keep`-ed `Run` that re-arms itself
  until it reaps or the worker settles. The scheduler stays ignorant of
  prompts, cron, and sessions;
  recurrence is not a scheduler concept — a recurring producer re-arms from
  inside its own `Run`, fired outside the heap lock so it cannot deadlock
  ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives]]).
- `reaper.rs` — the process-wide child watch, the one door onto a wait:
  `ChildHandle::into_watch` is how a spawned child reaches it, `Reaper::
  watch` how a bare pid does. A `SIGCHLD` handler on Unix (`reaper/unix.rs`)
  whose whole body is a write to a self-pipe, and a thread that per wake
  asks each watched pid two `waitid` questions — a stop, consumed and
  answered with `SIGCONT` on the spot so it never reaches a subscriber and
  is never re-reported on every wake, and an exit, observed with `WNOWAIT`
  and left as a zombie until `Watch::reap`, since the pid would otherwise be
  free for reuse while its owner may still signal it. *Not reaping is the
  point*: holding the zombie is what closes pid reuse structurally, rather
  than by care. Windows watches the same shape (`reaper/windows.rs`) via
  `RegisterWaitForSingleObject`, where a stop cannot arise. Every subscriber
  names its own event type — the pipeline collector's `Event::Ended`, the
  standalone command's `ChildEvent::Ended`, `spawn_detached`'s own channel —
  over `WaitOutcome`, the reaper's one wire type.
- `cancel.rs` — the cause-bearing `CancelScope` tree (`DurableRoot` /
  `ForegroundScope`, `CancelCause`) for structured-concurrency cancellation,
  polled cooperatively in hot loops
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]). A scope's
  cancellation is a *join*: one private `fold` over its chain's flags and the
  ambient causes the nodes were minted folding (`Hears`), so a signal handler
  or TUI thread contributes a cause without holding — or aliasing — a scope
  ([[decisions/260726_cancel-is-a-join|cancel-is-a-join]]). The two ambient
  causes have different shapes: `REQUESTED_ROOT` is an absolute `AtomicU8`,
  while the interrupt is a per-cause watermark of instants (`CLOCK`,
  `STAMPED`) a frame reads against its birth
  ([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).
- `signal.rs` — *signals are causes*: the platform handlers translate each
  delivered signal into a `CancelCause` on the ambient causes — SIGINT →
  foreground `Interrupt`, SIGTERM/SIGHUP → root `Terminate` — so one
  cancel-aware wait loop serves user interrupts, timeouts, and termination
  alike ([[decisions/260706_signals-are-causes|signals-are-causes]]).
  `check` is scope-only; `clear` is the boundary acknowledgment; the
  `ESCALATION` counter backs only the third-delivery `_exit` ladder
  (`escalation_pending` is the probe). A raw-mode frontend's Esc drives the
  same non-escalating foreground cancel
  ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
  Also `Pgid` /
  `PgidPolicy` / `ChildHandle` and the platform `spawn_with_pgid` family for
  process-group placement. Unix `ForegroundGuard` takes the `&TerminalLease`,
  performs the `tcsetpgrp` handoff, snapshots and restores tty foreground /
  termios, and blocks SIGTTOU for the parent-only restore window; unix
  `interrupt_foreground_child` re-sends raw-mode Esc/Ctrl-C to a foreground
  external group, `relay_handler` fans SIGINT to active external pgids, and
  `quit_handler` is the Ctrl-`\` root abort. Platform handlers live in
  `signal/unix.rs` and `signal/windows.rs`. Every Unix child wait now goes
  through the reaper's `waitid`, the one funnel; a pgid is signalled directly
  by `kill(-pgid, …)`, never waited on
  ([[decisions/260720_total-wait-status|total-wait-status]], superseded on
  the pid side). The Windows side
  carries the console-control escalation ladder (`CTRL_BREAK_EVENT` fan-out, then
  `TerminateJobObject`, then exit), `relay_interrupt` — `relay_handler`'s
  non-escalating twin, whose fan-out skips a detached worker's group — and
  `break_pipeline_group`, the SIGTERM-grade cooperative break a job teardown
  sends before escalating to `kill_pipeline_group`.
- `launch.rs` — the owned launch value and its platform interpreters, and the
  two births: `spawn`, which hands back a child this process owns, and the
  `cfg(unix)` `spawn_detached`, a double-fork whose grandchild is reparented
  to init — its pid comes back but nothing else does, so there is no handle,
  no wait, and (below) no `JailCgroup`. Unix
  lowers to `std::process::Command` and keeps the `pre_exec` pgid/fd discipline;
  Windows owns the raw `CreateProcessW` boundary, including command-line/env
  rendering, explicit helper-handle allow lists, the `SECURITY_CAPABILITIES`
  attribute a confined spawn attaches (its projection's AppContainer SID and
  the per-path fs and network capability SIDs —
  [[map/core/capabilities|capabilities]]), the launch mutex,
  suspended create → Job Object assignment → resume, and the widened
  `ChildHandle` raw process wrapper
  ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).
  The whole stop-work flow — the
  `Interrupt < Explicit < Deadline < Terminate < RootAbort`
  order — is narrated in
  [[internals/cancellation|cancellation]].
- `jail.rs` — the *guest spawn jail*'s decision layer: `GuestJail::plan`
  mints each exec a fresh unprivileged uid/gid and a fresh transient cgroup
  under `JailLimits` (memory / pids / CPU), with no syscall in the plan.
  Each `GuestJail` is one per booted engine — a hatched wire-seat child
  ([[design/agents|agents]]) installs its own, beside the trunk's — so the
  sequence number behind both the uid and the cgroup name is minted
  **guest-globally**, not per-engine: `linux::next_guest_seq` locks a
  counter file (`/run/ral/jail.seq`, `O_CREAT`, read-increment-write under
  an exclusive `flock`, released when the file drops) so every booted
  engine's jail mints off the one number line and two engines' first
  spawned commands cannot collide on a uid or a cgroup name. The cgroup
  path carries the engine's own pid as a grouping label, not a uniqueness
  guarantee (the sequence file already gives that): `ral-exec/engine-<pid>/
  exec-<seq>`, so a hatched child's teardown stays scoped to its own
  engine's tree and pid recycling can at worst hand a later engine a dead
  engine's stale directory name, which its own teardown tolerates. Off a
  real Linux guest, where `GuestJail` is never actually installed, `plan`
  falls back to a plain in-process counter so the module's own tests stay
  portable — `linux.rs` remains the only place a decision reaches the
  kernel. `jail/linux.rs` is the thin platform edge that
  realises a `JailPlan` — the cgroup tree and limit writes, the pre-exec
  supplementary-group clear / `setresgid` / `setresuid` / `NO_NEW_PRIVS`,
  kill-whole via `cgroup.kill`, and `EBUSY`-polled removal. The tree is
  built by descent, and the reason is a cgroup2 rule: a controller's files
  appear in a child only once the parent's `cgroup.subtree_control` enables
  it, so `memory.max` at the leaf needs an unbroken chain of enabling
  parents up to the cgroup2 mount itself. `make_delegated` therefore climbs
  to the first ancestor that already exists — the mount root at the latest,
  which is exempt from the no-internal-process rule and so may delegate
  while the daemon and engine still live in it — and enables on the way back
  down, one level ahead of each mkdir. A parent that will not delegate is
  refused outright rather than tolerated: a jail whose limits cannot be
  written must not run the command uncapped instead. `JailCgroup`
  is plain data `RunningChild` carries uniformly (`None` off a real
  guest); a detached birth is handed none at all, since a caller that
  cannot name the process cannot know when its cgroup is empty — the
  survivor keeps the transient cgroup's limits and leaves one inert
  directory until the guest reboots. The jail itself is session state (`session.guest_jail`),
  inherited exactly like the builtin table so workers, pipeline stages,
  and forks share one counter; only a guest engine (`RAL_GUEST`) installs
  it, and there it replaces the per-command OS projection
  ([[map/core/runtime|runtime]], `docs/SPEC.md` §12.11).

  **The recorded gap.** The jail is uid + cgroup + `NO_NEW_PRIVS` — there is
  no seccomp filter, so nothing stops a jailed process from calling
  `socket(AF_VSOCK)`. What that would buy it is a race with the host for the
  ephemeral guest port a parent engine binds while spawning a child
  ([[map/core/transport|transport]], [[map/synod|synod]]), and so a child
  engine seeded with the parent's scope — impersonation, not escalation,
  since grant narrowing is a plain meet and a run's capabilities are enforced
  inside the guest engine regardless of who computes them. Two things stand in
  the way, and only the first is confinement. A jailed process cannot dial a
  *guest-local* vsock port at all — measured 2026-08-24: the guest kernel
  refuses `VMADDR_CID_LOCAL` with `ECONNRESET`, and `/dev/vsock`, the only way
  to read one's own CID, is `EACCES` under the jail. That refusal is the
  standing defence. Behind it stand the eight token bytes the host must write
  before the listener will hatch anything: minted from the OS entropy source,
  held by the thread that owns the listener, dead when that one spawn ends.
  The token is the second line, against a jailed process that *guesses* a CID
  rather than reading one. A seccomp address-family filter
  that closes this at the syscall is unbuilt debt, not a design gap: the jail
  plan above has room for it (one more syscall-time check beside
  `NO_NEW_PRIVS`).

Spawning an external command is capability-gated; that gate lives in
[[map/core/capabilities|capabilities]], and the command/pipeline dispatch that
drives this plumbing in [[map/core/runtime|runtime]].

## Stream — `core/src/stream.rs`

Shared label vocabulary for the lazy Stream protocol: runtime variant labels
`more` / `done` and the `head` / `tail` payload fields, with the type-row
spellings (`` `more `` / `` `done ``) kept beside them so runtime and
[[map/core/typecheck|typechecker]] recognition cannot drift. `docs/SPEC.md` §14.5
covers Stream semantics.
