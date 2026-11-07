---
generated_at_commit: 1da6d2f91f3bffd25a41d4a75c0a662055955dd1
generated_at_date: 2026-11-04
covers_paths: [core/src/io/, core/src/io.rs, core/src/process/, core/src/process.rs]
---

# Map: core / IO & process

The byte plumbing under the [[map/core/evaluator|evaluator]]'s pipelines and
external commands: where a stage's bytes come from and go, the signals and
process groups that govern a foreground child, and the daemon that fires every
scheduled action. **Authority over the controlling terminal is carried as a
value, not re-derived from process state** — the foreground handoff is gated on
a held [[map/core/shell-state|TerminalLease]].

Windows process creation immediately owns both returned handles as
`OwnedHandle`; failed Job Object assignment or thread resumption releases both.
The child keeps the process handle, and the spawning thread releases the thread handle.

## IO — `core/src/io/`

`io/frame.rs` owns length-prefixed JSON framing for seeds and protocol
channels ([[map/core/transport|transport]]). Process syscall-site declarations
live in `process/site.rs`, beside the launch, wait and jail operations they name.

`io/state.rs` holds `Io`, the per-`Shell` streams (stdin / stdout / stderr),
where `stdout` is wherever the running computation writes — a capture buffer,
inside a capture. The terminal snapshot is the `Runtime`'s; interactivity and
*`SessionState::stage: Option<Membership>`* are the session's, stated by each
child's birth — `None` at the top level, the orchestrator;
`Some` for a stage's own children, carrying the group an external spawned
anywhere inside that stage must join — at the stage's own root or nested
arbitrarily deep, thread or process alike — and the stage scope whose causes
the group's owner delivers, so the member's own teardown opens only with a
cause that scope does not hold.
It decides pgid *placement* (a top-level standalone external may lead its own
group so a watchdog cancel can `kill(-pgid, …)` the whole subtree; anything
inside a stage joins that stage's pgid) and says whether a child's reader is
the caller or the next stage — never who may foreground. `shell/exec/pipeline/launch.rs`'s
`stage_stdin` resolves a stage thread's stdin against its route — the upstream
edge, or (`ByteIn::Parent`) a duplicate of the parent's own `shell.io.stdin`
— never taking or moving it, since `Source::reader` only ever hands out a
duplicate ([[internals/pipeline-execution|pipeline execution]]).

- `source.rs` — `Source`, a stage's byte input: `Terminal` (fall through to
  fd 0), `Empty` (no input — immediate EOF, child stdin to `/dev/null`, *no*
  fall-through to fd 0), and `Reader(SourceReader)` — a pipe (an upstream
  stage, a here-string) or a file (a `<file` redirect), carrying the scope a
  blocked read samples between slices (`poll.rs`'s `poll_slice`); a cause in
  force reads as EOF, never as an interrupted-read error a retry loop would
  spin on. `Source::reader` never *takes* a source: it hands every consumer —
  a builtin, an external's stdin, a nested stage — a duplicate, the scope left
  behind when a child inherits it, so a stage's
  stdin outlives every command that reads it. `Empty` is also what a
  tty-owning pipeline's stage 0 resolves to when it has no upstream reader —
  a thread in the shell process cannot read a terminal whose foreground
  belongs to the externals' group — and what an exarch tool run installs so a
  tool command can never steal the TUI's terminal; it is kept distinct from
  `Terminal` precisely so denial of byte input and denial of foreground stay
  separate effects.
- `sink.rs` — `Sink`, byte output and child stdio routing (`child_stdio`):
  terminal, stderr, redirect file, in-memory `ByteBuffer` capture, tee, a
  watched worker's line surface (`Watch`, each whole line handed to a line
  function the builtin builds around `WatchEvent`, so `io` knows no deferred
  sink), a stage thread's pipe edge (`Sink::Pipe { writer, edge, stage }`,
  `stage` naming the writing stage's node for its reader-gone stop; the scope
  a write polls is handed in, `write_under`). `child_stdio(fd)` is the one
  (stdio, pump) decision: a child inherits iff the sink is the inherited `fd`,
  a `File` or `Pipe` hands over its fd, and anything else is piped and pumped,
  so no caller computes inherit-vs-pipe by hand; `same_destination` says whether two sinks deliver to one destination,
  which decides whether a child's stderr may share stdout's descriptor.
  A `ByteBuffer` is `Arc<CapturedBytes>`: the prefix and its total byte count
  under one mutex, with a retention cap and no marker. Its readers are `take`,
  which drains the prefix into a `Retained` (prefix and total, whole iff they
  agree), and `peek`, which copies the prefix without draining (`poll`).
  `new_buffer` keeps 16 MiB; the audit tee retains its caller's smaller prefix.
  The write path cannot report the cap — a pump returns `()` from its own
  thread — so the `Retained` is read once the writers have joined, and each
  reader decides what a cut means ([[design/capture|capture]]).
  `streams.rs` — `Streams<T> { stdout, stderr }` with `map`, `as_ref`, `both`,
  `unzip` and `named`: every per-stream pair (a run's capture, a handle's
  buffers and `CompletedHandle`, the audit tee, `Captured`, `fact::Io`,
  `AuditIo`, `Pumps`).
- `poll.rs` — `poll_slice`, the one sliced fd wait (Unix): wait for readiness
  for at most `POLL_SLICE`, `EINTR` retried against the slice's own deadline.
  A blocked pipe write (`Sink::write_under`) or read (`read_interruptible`)
  is a loop of (read the scope; one slice), so another thread ends it by
  striking a scope, never by waking it. Windows cannot poll an anonymous
  pipe: the thread waiting on the blocked one aborts its call (`interrupt`,
  `CancelSynchronousIo`), the collector a thread stage's and
  `Pumps::join_under` a pump's, and the aborted read or write (`aborted`)
  re-reads its scope and, under a shield, retries; writes go a 4 KiB chunk
  at a time, so the sentinel's one read lands one
  ([[internals/cancellation|cancellation]]).
- `ansi.rs` — terminal escape vocabulary only: SGR constants, OSC builders,
  and escape-sequence scanning. Whether a sequence may be emitted is
  `terminal`'s decision.
- `terminal.rs` — `TerminalState`: cached startup isatty / ANSI / NO_COLOR /
  mode bits. `startup_foreground` records whether ral's group owned the
  controlling terminal's foreground at entry; it is not a per-handoff
  oracle but the lease's *mint condition*
  ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
  On Windows it also owns `console_mode_snapshot` / `restore_console_mode`,
  the termios-snapshot analogue a panic hook restores a raw-mode console
  through.

Redirect reads and writes — `< file`, `> file` and friends — open through the
`File` source/sink here, and the runtime emits a byte-level I/O door at each:
the read fires eagerly when stdin is redirected, and every write at frame settle
with its target before and after — a stream `committed` whatever its body did,
an atomic `>` committed, aborted or failed. The event shapes and their card
rendering belong to [[map/exarch/io-surface|io-surface]].

## Process — `core/src/process/`

- `outcome.rs`: `Signal` and `WaitOutcome`. `WaitOutcome::cancelled_by` names
  the cause for a death by one of the cause's `signals_of` (grace, key, kill),
  read off the one reader `WaitOutcome::death`, and `WaitOutcome::failure` is
  the `fact::Failure` of any other end, which `command::process::ending`
  turns into the run's stop or failure; no death mints a cause, so a torn-down child reports the cause,
  an expired time limit, a `cancel`, an interrupt or a shutdown, exactly as a
  poll point does, while a signal from outside ral still reports its number
  ([[internals/cancellation|cancellation]]). `cancelled_by` takes `cause: Option<
  CancelCause>`, the strongest cause in force when the child ended, joined by `max`
  where two parties each ended it — a cancellation in force outranks the
  collector's reader-gone kill (raised only once the sentinel hears a dead
  write) by that order rather than by a special case. It is the sole input to
  forgiveness for an external: only `Some(ReaderGone)`, and only a death that
  kill actually caused, keeps no failure; every other status is kept, because
  the kill precedes the wait and cannot rewrite a recorded status
  ([[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]],
  [[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]]).
- `spawn_lock.rs` — platform-specific process-wide spawn locks. Apple's `RwLock`
  closes the fork/`CLOEXEC` race: neither `pipe2` nor `SOCK_CLOEXEC` is
  atomic there, so a `fork` racing a pipe's create-then-`fcntl` window can hand
  an exec'd child a fd it should never have inherited. `cloexec_pipe` /
  `cloexec_socketpair` take the shared side; `spawn(cmd)` is the *only* fork
  door, holding the exclusive side around `Command::spawn` and nothing else —
  a `.output()`/`.status()` caller must spawn through it and wait outside the
  lock, or a child's whole lifetime would block every other thread's fd
  creation. `clippy.toml`'s `disallowed-methods` enforces both doors on this
  target. Windows' `lock_inheritance` mutex covers raw `Launch` creation and
  temporary inherit-flag restoration alongside helper-backed `Command::spawn`;
  it ends before Job Object placement/resume and waits. An explicit handle list
  restricts the raw child only, not another inheriting child; dependency spawns
  outside ral's helpers remain outside this lock. Linux has no lock.
- `lease.rs` — `TerminalLease`, the unforgeable authority to hand the
  controlling terminal to a child via `tcsetpgrp`. No public constructor,
  neither `Clone` nor `Copy`: a host cannot forge or duplicate it. Minted at
  most once at session construction iff ral owned the foreground at startup
  (`None` on a backgrounded or tty-less Unix launch; Windows mints from
  console attachment), then lent per run as `&TerminalLease` to the one chokepoint
  that foregrounds — `TerminalLoan::prepare`, which is *uninvocable*
  without the borrow. Preparation captures terminal state before spawn;
  `PreparedTerminalLoan::lend` performs the post-spawn handoff. The type lives at [[map/core/shell-state|shell-state]];
  the rationale at [[decisions/260619_terminal-lease|terminal-lease]].
- `deadline.rs` — one lazily started, process-global daemon (`ral-deadline`)
  owning a min-ordered heap of `(when, action)` entries, firing each at its
  `Instant`. *Deadlines are data*, not a thread per worker: `arm_lifetime` /
  `arm_callback` push an entry and return a `#[must_use]` `Deadline` guard —
  dropped, the entry disarms; `keep`-consumed, it fires regardless (the
  fire-and-forget mode for actions that outlive their arming call).
  The fired action is `Cancel(scope) | Run(closure)`: `Cancel` cancels a
  `ScopeNode` with `CancelCause::TimedOut` (the foreground wall); `Run`
  invokes an opaque host closure once, the shape a scheduled
  wakeup rides — exarch arms a `Run` that posts a prompt and wakes its idle
  loop, and an agent worker arms a `Run` that cancels its own token at
  its ceiling. The scheduler stays ignorant of
  prompts, cron, and sessions;
  recurrence is not a scheduler concept — a recurring producer re-arms from
  inside its own `Run`, fired outside the heap lock so it cannot deadlock
  ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives]]).
- `reaper.rs` — the process-wide child watch, the one door onto a wait:
  `ChildHandle::into_watch` is how every spawned child reaches it,
  `spawn_detached`'s intermediate included, `reaper::watch` being private to
  `process`. `Watch::kill` is `signal(KILL)`. A `SIGCHLD` handler on Unix
  (`reaper/unix.rs`), without `SA_RESTART`, whose whole body is a write to a
  self-pipe, and a thread that per wake
  asks each watched pid two `waitid` questions — a stop, consumed and
  answered with `SIGCONT` on the spot so it never reaches a subscriber and
  is never re-reported on every wake, and an exit, observed with `WNOWAIT`
  and left as a zombie until `Watch::reap`, since the pid would otherwise be
  free for reuse while its owner may still signal it. The exit code is masked
  to a byte, as `waitpid` would report it: Darwin's `waitid` returns all of
  `exit()`'s argument, so macOS bash 3.2's syntax-error 258 would otherwise
  surface as 258 instead of 2. *Not reaping is the
  point*: holding the zombie is what closes pid reuse structurally, rather
  than by care. Windows watches the same shape (`reaper/windows.rs`) via
  `RegisterWaitForSingleObject`, where a stop cannot arise. Every subscriber
  names its own event type — the pipeline collector's `Event::Ended`, the
  standalone command's bare `WaitOutcome` channel, `spawn_detached`'s own —
  over `WaitOutcome`, the reaper's one wire type.
- `cancel.rs` — the cause-bearing scope tree (`Scope` read, `ScopeNode` written,
  `DurableRoot`, `CancelCause`) for structured-concurrency cancellation,
  polled cooperatively in hot loops
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]). A scope's
  cancellation is a *join*: one private `resolve` over its chain's flags,
  naming the outermost node that attains it (`ScopeId`) and masking at a
  shielded node (`shielded`) within the grace from the chain's first strike.
  `Scope::poll` returns `Result<(), Cancellation>` — cause and owner, no
  runtime error or escape. `shell/runtime/flow.rs` converts it to a
  non-catchable `Break::stopped`, preserving both; `Shell::poll` is that adapter.
  `POLL_SLICE` is every sliced wait's period. A
  signal handler raises an ambient cause instead — `REQUESTED_ROOT`, an
  absolute `AtomicU8`, or an interrupt on the `INTERRUPTS` count — which no
  scope folds: `forward_ambient` hands each to a host thread, which forwards it
  to its engine as `Control`; its `AmbientForward` guard, on drop, unregisters,
  drains the queued causes and joins that thread, so no callback outlives it
  ([[internals/cancellation|cancellation]]).
- `fatal.rs` — `Fatal<T>`, a terminal failure latched once: the first cause
  stands. Latching cancels the bound `ScopeNode` with `Cancelled` under the
  lock that records the cause, so a standing cause implies an ended life.
  A life bound after the latch is cancelled at once; `unbind` releases one
  unended. The carriers' severance and exarch's record writer are each one.
- `signal.rs` — *signals are causes*: the platform handlers raise each
  delivered signal as an ambient cause a host forwards — SIGINT → foreground
  interrupt, SIGQUIT → root `Aborted`, SIGTERM/SIGHUP → root `Terminated`, a
  closed `RootCause` read from the `GESTURES` key table — so one
  cancel-aware wait loop serves user interrupts, timeouts, and termination
  alike ([[decisions/260706_signals-are-causes|signals-are-causes]]).
  The poll point is `Shell::poll`, scope-only; `clear` is the boundary
  acknowledgment; the `ESCALATION` counter backs only the third-delivery
  `_exit` ladder (`escalation_pending` is the probe), whose `_exit` reads
  the signal's cause's code. A raw-mode frontend's Esc drives the same
  non-escalating foreground cancel
  ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
  `signals_of` is one function over each platform's `grace_signal`,
  `gesture_signal` and `KILL`. Unix's one install is `install(sig,
  Disposition)`: `Interrupt` (a bare `request_interrupt()`, with no delivery
  of its own, since a pipeline's processes hear a cancellation through the
  collector), `Shutdown` (the signal's cause, ticking the ladder), `Abort`
  (the signal's cause, unescalated: Ctrl-`\`), `Ignore`, `Default`. None
  sets `SA_RESTART`, so a blocked call a handler cuts returns `EINTR` and ral
  retries it; the first install snapshots the inherited ignored
  dispositions. `grace_signal(cause)` is the one
  cause→signal table the one teardown reads (the collector's `cancel_all`):
  `Interrupted` → SIGINT,
  `Cancelled`/`TimedOut`/`Terminated` → SIGTERM (on Windows all four →
  Ctrl-Break, named by its `STATUS_CONTROL_C_EXIT`), `ReaderGone`/`Aborted`
  → `None`, straight to the kill
  ([[decisions/260905_one-delivery-path|one-delivery-path]]).
  Platform handlers live in `signal/unix.rs` and `signal/windows.rs`; the
  Windows side carries the console-control ladder in the Unix shape (two
  `request_interrupt()`s, then `ExitProcess`), which signals no process.
- `child.rs` — `ChildHandle`, the spawned child, and `into_watch`, the one
  door from it to the reaper, where a Linux child's `Testimony` amends the
  reaped outcome before any subscriber sees it
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
  Every Unix child wait goes through the
  reaper's `waitid`, the one funnel
  ([[decisions/260720_total-wait-status|total-wait-status]], superseded on
  the pid side).
- `group.rs` — `Pgid` / `PgidPolicy` and the platform `spawn_with_pgid`
  family (`group/unix.rs`) for process-group placement, and the two facts of
  who signals a group: `Membership` (a joined pipeline group and its stage
  scope) and `Group` (`Owns(Pgid)`, signalled and killed whole, or
  `Joins(Membership)`). A pgid is signalled directly by `kill(-pgid, …)`,
  never waited on. On Windows (`group/windows.rs`) a group is a Job Object
  and `break_pipeline_group` is the grace an owned group's teardown sends —
  the collector's `Address::signal` for a group — before escalating to
  `kill_pipeline_group`;
  a pid gets the kill alone.
- `foreground.rs` — `TerminalLoan::prepare` takes the `&TerminalLease` and
  run's node (`ScopeNode`) before spawn on every platform, returning a
  `PreparedTerminalLoan`; `lend` follows spawn. Unix preparation snapshots
  foreground / termios, then lending performs `tcsetpgrp`. The loan blocks
  SIGTTOU during parent-only restoration and hears the tenant's key back
  (`hear`, `pressed`, `reclaim`), striking it on the run's node.
  Windows preparation duplicates console handles and snapshots their modes,
  restoring the same console objects on reclaim or drop. Prepared and lent
  guards both restore on failure and unwind. It does not transfer
  console ownership or isolate a child from the shared console.
  `interrupt_foreground_child` re-sends raw-mode Esc/Ctrl-C to a foreground
  external group.
- `limits.rs` — what a child spawned under an active grant may consume: no
  core dump, and a process budget (macOS's fork brake,
  `limits/fork_brake.rs`, a Windows Job Object cap). Resource-limit installation
  is checked; the macOS cap respects the inherited hard limit, and
  `limits::failure_hint` offers a conditional process-limit explanation for
  status 128 when no kernel denial was attributed.
  `Launch::limit_resources` arms the pre-exec half; on Windows the Job Object
  cap is set at the launch boundary (`launched`, `set_active_process_limit`),
  on the child's group's job or a job made for it alone. A budget that cannot
  be armed is an `Err`, never a warning: the launch is refused as
  sandbox-unavailable
  ([[design/two-enforcers|two-enforcers]]).
- `report.rs`: a confined launch's report socket: the `Record`s the trampoline
  writes (`Witness`), and the parent's `Testimony`, which reads the first word
  under the launch's scope and amends a Linux child's ending
  ([[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]],
  [[invariants/a-first-word-is-waited-for-under-the-scope|a-first-word-is-waited-for-under-the-scope]]).
- `launch.rs` — the owned launch value and its platform interpreters, and the
  two births, each returning `Unstarted` for a launch that made no child
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]):
  `spawn`, which hands back a `Spawned` (the child, its `Landing`
  ([[invariants/a-child-lands-once|a-child-lands-once]]) and any jail cgroup),
  and the `cfg(unix)` `spawn_detached`, a double-fork whose grandchild is reparented
  to init — its pid comes back but nothing else does, so there is no handle,
  no wait, and no `JailCgroup` comes back — shared membership never owns one (below). Unix
  lowers to `std::process::Command` and keeps the `pre_exec` pgid/fd discipline;
  Windows owns the raw `CreateProcessW` boundary, including command-line/env
  rendering, explicit helper-handle allow lists, the `SECURITY_CAPABILITIES`
  attribute a confined spawn attaches (its projection's AppContainer SID and
  the per-path fs and network capability SIDs —
  [[map/core/capabilities|capabilities]]), the shared inheritance mutex above,
  suspended create → Job Object assignment → resume, and the widened
  `ChildHandle` raw process wrapper
  ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).
  The whole stop-work flow — the
  `ReaderGone < Interrupted < Cancelled < TimedOut < Terminated < Aborted`
  order — is narrated in
  [[internals/cancellation|cancellation]].
- `jail.rs` — the *guest spawn jail*'s decision layer: `GuestJail::plan`
  mints each exec a bounded `Seq` whose uid/gid is `BASE_UID + seq`,
  from 100000 through `u32::MAX - 1`, and cgroup membership under
  `JailLimits` (memory / pids / CPU), with no syscall in the plan.
  There is one `GuestJail` per guest process, installed once by the
  Linux x86-64/aarch64 `--guest-engine <cgroup-root>` role, named by
  `guest_abi::layout::ENGINE_FLAG` (`jail::install_guest`, requiring one
  absolute root and euid 0; the root is
  `guest_abi::layout::EXEC_CGROUP`, which the daemon makes and delegates into
  at boot; both use `layout::EXEC_CONTROLLERS`. The jail makes
  `engine-<pid>/exec-<seq>` below it, or the shared `detached` leaf for every
  survivor, and the daemon removes a dead engine's tree) and read by
  `jail::guest()`; a sealed wire-seat child engine ([[design/agents|agents]]) is
  its own process and installs its own, so the
  sequence number behind both the uid and the cgroup name is minted
  **guest-globally**, not per-engine: `linux::next_guest_seq` locks a
  counter file (`/run/ral/jail.seq`, `O_CREAT`, read-increment-write under
  an exclusive `flock`, released when the file drops) so every booted
  engine's jail mints off the one number line and two engines' first
  spawned commands cannot collide on a uid or a cgroup name. `Count`
  stores the issued count: empty at birth, then ten zero-padded ASCII digits;
  corruption or exhaustion refuses the launch without writing. A fixed-width
  positional rewrite torn mid-write leaves the old, unissued count or at
  least the new count; a partial birth is corrupt, so neither reuses an
  issued uid. The cgroup
  path carries the engine's own pid as a grouping label, not a uniqueness
  guarantee (the sequence file already gives that): `ral-exec/engine-<pid>/
  exec-<seq>`, so a child engine's teardown stays scoped to its own
  engine's tree (the daemon kills a dead engine's commands through `engine-<pid>/cgroup.kill`, since they leave its process group) and pid recycling can at worst hand a later engine a dead
  engine's stale directory name, which its own teardown tolerates.
  `plan` takes the launch's `authority::sandbox::Ownership`: a `Surrendered` (detached)
  exec goes to the one `ral-exec/detached` leaf instead, out of every engine's
  tree. All survivors together receive `JailLimits::default()` (2 GiB memory,
  256 tasks, 200% CPU); distinct uids still separate them. An engine's end
  spares its survivors as it does on the host; the daemon's halt reaches them. Off a
  real Linux guest, where `GuestJail` is never actually installed, `plan`
  falls back to a bounded in-process counter so the module's own tests stay
  portable; `linux.rs` remains the only place a decision reaches the
  kernel. `jail/linux.rs` is the thin platform edge that
  realises a `JailPlan`: the cgroup tree and limit writes; pre-exec cgroup
  placement, the offline network namespace with loopback up, supplementary-group
  clear / `setresgid` / `setresuid` / `NO_NEW_PRIVS`, descriptor marking above
  stderr with `close_range(CLOSE_RANGE_CLOEXEC)`, then the envelope's
  [[decisions/260906_seccomp-is-a-typed-deny-set|seccomp deny-set]];
  kill-whole via `cgroup.kill`, and `EBUSY`-polled removal.
  `Launch::apply_guest_jail` resolves the programs before preparation; the
  pre-exec hook neither allocates nor locks, and any failure refuses launch.
  The offline-network test probes user then network namespace creation before
  checking isolation; it skips when unprivileged user namespaces are refused
  or granted without capabilities, as AppArmor can enforce.
  Descriptor marking needs Linux 5.11 and keeps std's error pipe and detach's
  handshake writable until exec; the later pgid and detach hooks run under
  the deny-set. The tree is
  built by descent, and the reason is a cgroup2 rule: a controller's files
  appear in a child only once the parent's `cgroup.subtree_control` enables
  it, so `memory.max` at the leaf needs an unbroken chain of enabling
  parents up to the cgroup2 mount itself. `make_delegated` therefore climbs
  to the first ancestor that already exists — the mount root at the latest,
  which is exempt from the no-internal-process rule and so may delegate
  while the daemon and engine still live in it — and enables on the way back
  down, one level ahead of each mkdir. A parent that will not delegate is
  refused outright rather than tolerated: a jail whose limits cannot be
  written must not run the command uncapped instead. `JailPlan` carries
  `Ownership`: after delegation and before fallible setup, `prepare` constructs
  `Some(JailCgroup)` only for a kept command. Dropping it kills whatever is left
  (`cgroup.kill`) and removes its leaf, so a setup, wiring or spawn failure,
  pipeline rollback or settled child finishes it once. `Launch` and an external
  stage carry that option uniformly (`None` off a real guest).
  Shared detached membership never constructs a teardown guard: failed
  preparation, failed spawn and abandoned launch cannot kill existing survivors
  or remove their leaf. The shared limits are rewritten idempotently without
  delegating controllers below `detached`; there are no per-survivor cgroups
  to reclaim. The jail itself is process-global (a `OnceLock` in `jail.rs`),
  so workers, pipeline stages, and forks share one counter; only a guest
  engine (`--guest-engine`) installs it, and there it replaces the per-command
  fs/exec projection for the admitted utility and its descendants, while
  preserving offline network isolation and seccomp
  ([[internals/capability-enforcement#Guest jail|guest jail boundary]],
  [[map/core/runtime|runtime]], `docs/SPEC.md` §12.11).

  The jail refuses `socket(AF_VSOCK)` with `EPERM`; the daemon's factory
  ([[map/synod|synod]]) admits only the kernel-written `VMADDR_CID_HOST`
  peer identity as the second line behind it.

Spawning an external command is capability-checked; that in-process guard
lives in
[[map/core/capabilities|capabilities]], and the command/pipeline dispatch that
drives this plumbing in [[map/core/runtime|runtime]].
