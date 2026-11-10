---
generated_at_commit: 40af72fb
verified_at_commit: 40af72fb
verified_at_date: 2026-11-09
anchors: [StartFailure, StartFailure::settle, Command::prepare, Prepared::start, Prepared::run, Prepared::detach, Started::register, Started::collect, ChildSetup::install, Program::unstarted, ending, exit_error, Started, denial_pids, payload_pids, Unstarted, Spawned, Group, Placement, Placement::land, Placement::command, TerminalAccess::reach, TerminalHold, PreparedTerminalLoan::lend, Unlanded, Drains::join_under, SpawnError, spawn_with_pgid_after, attributed, fork, Launcher, Launch::born, Leader, EnvelopedLaunch, ProgramLaunch, unprepared, vanished, Confined, Testimony, Witness, Record, Program::exec_error, offer_path_as_text, interpreter_of, interpreter_hint, supervise, into_watch]
---

# An external, end to end

**A command owns its preparation and start; a collector owns its running
child.** `Prepared` in `core/src/shell/exec/command.rs` uses the process
layer in `core/src/process/launch.rs`. Lone commands and direct pipeline
stages share startup, pump setup, reaper registration and collection. Their
streams, placement and audit context differ. A detached command shares
preparation and failure interpretation, but returns no kept child. What the operation means
is [[design/exec|exec]]; what a confined launch adds is
[[internals/confined-launch|confined-launch]].

## Three stages

| stage | function | it does | its failures leave as |
|---|---|---|---|
| prepare | `Command::prepare` | reads the head's verdict, vets argv and owns the launch, its image, interpreter and confinement context; `detach` alone asks for a surrendered one | the head's verdict; a refused argument; a preparation error |
| start | `Prepared::start` / `run`, `Prepared::detach` | consumes the command, connects its streams (a survivor's to null), and spawns under the scope; a kept result owns the child, opaque terminal hold, pumps and denial PIDs, a detached result is `Survivor` | `Program::unstarted`, the one conversion of `Unstarted`, below |
| ending | `ending` | a death by a signal of a cause in force is that cause's stop; any other end is a process failure (`Reason::process`), with the kernel's account of the deny lines its start claimed | a stop, or a process failure |

- **Every failure of a stage has one home.** Preparation raises its refusals as
  the `Error`s its building meets; `unstarted` is the only place a launch that
  returned no kept child becomes the run's error; `ending` is the only place
  a child's end does. A fourth way to raise is a defect.
- **Installation fixes child setup order once.** A launch's child steps are
  data, its `ChildSetup`, whatever order they were configured in; the spawn's
  `ChildSetup::install` registers them as one hook: merge stderr, capture
  payload streams, apply session limits, then mandatory limits and
  confinement. Descriptor handoff precedes it; placement, signal reset and the
  exec-readiness marker follow it. The parent drops child-only descriptors
  right after the fork, before waiting for the payload report.
- **Building describes; starting acts** ([[design/exec|exec]]). Two things in
  `prepare` touch the world: a guest jail's cgroup, which `JailCgroup::drop`
  removes if the built launch is dropped, and Linux's bwrap host probe, which
  waits on bwrap children once per process. Whatever may be stopped, Windows'
  stamps and token and the wait for a trampoline's first word, is
  the direct or enveloped spawn's, under the scope `start` hands it. On Unix the scope is read
  once, before the fork; on Windows before confining and again just before the
  child is made, so a wall that expired during minutes of stamping is found
  before the child exists. So `start` is the one place a launch is stopped or
  fails as a program.
- **Where a child ended is amended before `ending` sees it**
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).

## How a start fails

**`Unstarted` says whose failure it was, and the answer is never inferred from
an `errno`.** Direct and enveloped spawns return the same failure type.

Which of the three cases a failure is, and so whom it blames, is
[[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]].
`Stopped` arises from `Launch::born`'s poll, `dacl::cancelled` and
`Testimony::first`.

- **`Program` is read as the program's.** `Program::exec_error` turns it into the same
  process failure the probe reports, with the interpreter hint where it
  applies. `Refused` keeps its own wording (`unprepared`: "ral could not
  prepare the command's process"; "cannot start <launcher>").
- **The mark decides it**: a spawn error that the child's last pre-exec step
  did not precede is ral's
  ([[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]). `SpawnError` is
  `Prepare` or `Exec`, and `fork` reads it: a `Prepare` failure is always
  ral's own (`unprepared`), whatever stands in the program's place. `Exec`
  is `Program`, unless the launch holds a `Launcher` to run in the program's
  place: then it is that launcher's, and `Refused`. Only the fork is read so.
  A payload report's `Program(errno)` is heard afterwards and is never
  rewritten as a launcher refusal.
- **A discarded child is killed and reaped** (`Unlanded`). A refusal after the
  fork, such as an unnameable payload group or a refused report, leaves no
  running child behind. `born` holds the created child in an `Unlanded`, names
  an envelope's payload group into it before it hears anything, and retains
  the applied `Step` and request until landing completes. Unless the child
  lands, the guard's drop kills the payload's group, then the child's own
  fresh group when `Step::leads()`, before reaping the child, whose unreaped
  pid pins the group it leads: no exit from a birth skips it. `born` then
  drops the request, restoring captured state, and releases the jail. `detach` leaves its survivor: it is never
  killed.
- **A refused terminal handoff may follow effects.** `Launch::spawn` lends a
  direct tenant's fresh group after exec. Refusal retains the captured guard
  through group kill and child kill/reap, then restores state and releases the
  jail before returning `Unstarted::Refused`, naming the operation and OS error.
  It does not claim the command never ran.
- **A failed start owns its drains until caller settlement.**
  `Prepared::start` returns `StartFailure` with the reason and already-started
  drains: even a refused child may have written output. A lone `run` settles
  before propagating the reason. A pipeline first rolls back and joins its
  owner, even if no stage registered, then settles the failed-start drains
  before returning the error. A joined child's descendants are its enclosing
  owner's to kill, not the failed launch's. `StartFailure::settle` uses an
  empty kill callback, never a numeric ID retained after reap.
  `Drains::drop` remains non-blocking for unwinding.

## Which failure reads as what

**The spawn's `errno` says what the OS found; the image says what that means.**
`Program::exec_error` reads it against the `Image` the launch was built from, and
names where the program was found as lookup named it: the path as typed, or
the one a `PATH` search built.

- **`ENOENT` for an image that still exists on the host is `not-runnable ...
  missing-interpreter`.** The file is there; what it needs is not. The failure
  names the `#!` interpreter read with the image (`interpreter_of`, kept in
  the `Prepared` launch's `Program`), never what the file says by the time the kernel refused. A bundled tool, or a file gone
  from its real path, is `not-found`.
- **Only a permission refusal is `permission-denied`.** `EACCES` at the spawn,
  or a walk, a path head's or a `PATH` candidate's, that fails by a refusal.
  Any other walk error reads as itself, `io` with the OS's words; absence and
  a non-directory component are `not-found`.
- **A confined `EACCES` or `ENOENT` whose interpreter the launch's own
  projection hides is `permission-denied` plus the interpreter hint**, on both
  platforms: Seatbelt refuses the exec (`EACCES`), bwrap hides the interpreter
  (`ENOENT`), and the user sees one thing. `Prepared` keeps the launch's own
  confinement, and only on a failure to start is the verdict asked of it
  (`interpreter_hint`): the exec table's, then the read table's, so an
  interpreter excluded, not admitted to run, or outside the read set each
  names its fix; with no projection, as inside the guest jail, there is
  no verdict and so no hint, and a hidden interpreter reads as missing.
- **The path-as-text hint is one rule, at the machine's `dispatch_exec`**
  (`offer_path_as_text`). It is offered only to a path head given no arguments,
  most likely meant as text, to a refused admission and to a run that named a
  directory or plain file, never to another failure. It is a stored hint like
  any other, composed as `docs/SPEC.md` §13.1 says.

## The trampoline reports

**A confined launch's trampoline testifies on a report socket; the parent hears
the first word before it believes the program started.**
The records, the socket's discipline and why they hold are
[[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]].

- **The first word.** `Testimony::first` returns once the program runs. On
  macOS the trampoline `execve`s in place, so its close-on-exec writer closing
  is the start, and EOF before a record says so. On Linux the trampoline
  supervises, so EOF before a record is an envelope that died: `Refused`.
- **The wait is a cancellable one**
  ([[invariants/a-first-word-is-waited-for-under-the-scope|a-first-word-is-waited-for-under-the-scope]]):
  a stop ends it, and the child is discarded.
- **The parent's writer copy outlives the hook that names it.** `Handoff::keep`
  lends the child a copy of the report's writer at a fixed number and the
  spawn's `pre_exec` clears close-on-exec on that number, so the parent keeps
  the copy open until the fork and `Report::forked` drops it right after. A
  `Launch` is spent by its spawn (`spawn` takes `self`), since a second spawn
  would hear a first one's launcher end and read it as the program's.
- **What remains of the report on Linux rides the `ChildHandle`** as its
  `Testimony`, read at the reaper
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
  The supervisor that writes it is
  [[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]].

## Whose deny lines a failure owns

**A failure owns only the kernel deny lines its start claimed.** `start` takes
its `denial_pids` the moment the spawn returns, and `ending` hands `exit_error` the
`Failure` itself, never its rendering.

- **Confinement is preparation's fact**: a projection, or the guest jail.
  An unconfined child claims nothing, so its failure reads no log.
- **A line carries only the host pid the kernel denied**: Seatbelt's
  `comm(pid)`, and seccomp's audit `pid=`, which is the host's number even
  inside a pid namespace. So a claim is a set of pids, each read while its
  process lived.
  - A direct child (macOS, which `execve`s in place; the guest jail) claims
    its own pid, which is the program's.
  - An enveloped child (Linux) claims the chain beneath its payload's group
    leader: the namespace's init, the supervisor, the program. bwrap's
    monitor is confined by nothing, and no process inside the namespace can
    name the program's host pid, so `payload_pids` reads the chain from the
    host's `/proc/<pid>/task/*/children` right after the trampoline has said
    `started`, the one moment the program is known to run.
- **What a claim misses.** A process gone before its claim was read (a program
  that dies within microseconds of starting) or born after it (every later
  descendant, such as `sh -c 'cat x'`'s `cat`) is not claimed, and its line is
  not shown.
- **A line is never claimed falsely.** Each pid was read alive, so a line names
  it only if that very process was denied, short of the host reusing the pid
  within the window, which takes a wrap of its whole pid space.
- **No sample is taken after the end.** A process's children are reparented
  when it exits, not when it is reaped, and an envelope's namespace is dead
  before bwrap exits: such a sample finds nothing of the command, and one of
  ral's own live tree would claim a stranger's line.
- **The hint is decided from the datum**: the claimed lines first; else, on
  macOS, an `Exited(128)` gets the fork brake's conditional explanation. A death
  by a signal is never read as a status.

## Where the child landed

**A child's `Group` is computed once, at its birth, and carried.**
`Shell::group_owner` gathers stage membership or session facts once;
`Placement::command` selects `Join`, permitted `Tenant`, interactive `Inherit`
or headless `NewSession`. `Placement::input` determines the requested terminal
stdin before `Source::offered` interprets it. `Placement::land` derives every
ordinary direct `Group` from that request, after the platform has applied its
`Step`; platform code builds no group. Linux's `EnvelopedLaunch::spawn` calls
`born` with `Step::NewLeader` and lands on `Group::Enveloped`, the payload's
group; `Confinement::denial_pids` reads `payload_pids` from it. An eligible
envelope's `Tenant` keeps readable
stdin and a restoration-only hold, without handoff or key attribution.
Otherwise the direct tenant's landing mints a `Leader` and lends to `&Leader`.
The certificate proves fresh-group birth in ral's session, not liveness;
`spawn_leader` returns the same certificate to the anchor beside its child pin.
`Spawned<G>` varies only its group field. `Spawned.terminal` carries the opaque
hold to collection, which reclaims it before jail settlement and pump joins.
Nothing downstream recomputes the landing ([[invariants/a-child-lands-once|a-child-lands-once]]);
what each landing lets a collector signal is
[[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]]'s. The process limit is a
property of the launch, applied where the child is born (a macOS fork limit set
in `pre_exec`; on Windows the Job Object's, before the child resumes; Linux
sets none), so a child never runs without the budget its grant states
([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

See also [[design/exec|exec]], [[design/failure|failure]],
[[internals/exec-judgment|exec-judgment]],
[[internals/confined-launch|confined-launch]],
[[internals/cancellation|cancellation]],
[[map/core/io-process|io-process]].
