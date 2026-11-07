---
generated_at_commit: b9f3d1b708751d86428c306362f82a72d2904928
verified_at_commit: b9f3d1b708751d86428c306362f82a72d2904928
verified_at_date: 2026-11-07
anchors: [prepare, start, start_detached, unstarted_error, ending, with_denials, Started, Born, Claim, payload_pids, Unstarted, Spawned, Group, Placement, SpawnError, spawn_with_pgid_after, attributed, unforked, unprepared, vanished, Confined, Testimony, Witness, Record, at_spawn, offer_path_as_text, interpreter_of, interpreter_hint, supervise, into_watch]
---

# An external, end to end

**An external's life is one path in three named stages, and each stage raises
its failures one way.** The path is `core/src/shell/exec/command.rs`;
the process layer under it is `core/src/process/launch.rs`. A plain command, a
directly launched pipeline stage and `detach` each wire their own streams
between the first two stages and share nothing else. What the operation means
is [[design/exec|exec]]; what a confined launch adds is
[[internals/confined-launch|confined-launch]].

## Three stages

| stage | function | it does | its failures leave as |
|---|---|---|---|
| prepare | `prepare` | reads the head's verdict, vets the arguments (they are words), describes the launch (`Image`, the `#!` interpreter, the projection, env, cwd, limits) | the head's verdict; a `Refusal` as raised text |
| start | `start`, `start_detached` | take the `Prepared` by value and spawn it under the run's scope, confinement first, returning `Started { shown, args, born }`, where a kept start's `born` is the child and its `Claim`; the one conversion of `Unstarted` | `unstarted_error`, below |
| ending | `ending` | a death by a signal of a cause in force is that cause's stop; any other end is `Error::process`, with the kernel's account of the deny lines its start claimed | a stop, or a process failure |

- **Every failure of a stage has one constructor.** `From<Refusal> for Break`
  words every refusal of `prepare`; `unstarted_error` is the only place a launch
  that made no child becomes the run's error; `ending` is the only place a
  child's end does. A fourth way to raise is a defect.
- **Building describes; starting acts.** `prepare` waits on nothing a stop
  must end, so its one failure is a refusal. Two exceptions touch the world: a guest jail's
  cgroup, which `JailCgroup::drop` removes if the built launch is dropped,
  and Linux's bwrap host probe, which waits on bwrap children once per
  process.
  Whatever may be stopped, Windows' stamps and token and the wait for a
  trampoline's first word, is `Launch::spawn`'s, under the scope `start`
  hands it, which it reads before confining and once more just before the
  child is made: a wall that expired during minutes of stamping is found
  before the child exists. So `start` is the one place a launch is stopped
  or fails as a program.
- **Where a child ended is amended before `ending` sees it**
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).

## How a start fails

**`Unstarted` says whose failure it was, and the answer is never inferred from
an `errno`.** `Launch::spawn` and `spawn_detached` return nothing else.

| case | it is | it arises from |
|---|---|---|
| `Program(io::Error)` | the program's own failure to become a process | the `execve` of an unconfined launch; the trampoline's `unstarted(errno)`, only a file's own `execve`; on Windows only `CreateProcessW`'s own error |
| `Refused` | never the program's fault | a launcher or envelope that fails to exec; a payload process group that cannot be named; the trampoline's `refused`, which covers its own fork or setup failing; a Linux envelope that died before reporting; a step of ral's own before the `execve`, confined or not (on Windows also a `.bat` or `.cmd`, a NUL in an argument, handle admission) |
| `Stopped` | the run's stop, seen by the start: before the child is made, before a Windows stamp, or while waiting for the trampoline's first word | `Launch::spawn`'s poll; `dacl::cancelled`; `Testimony::first` |

- **`Program` is read as the program's.** `at_spawn` turns it into the same
  process failure the probe reports, with the interpreter hint where it
  applies. `Refused` keeps its own wording (`unprepared`: "ral could not
  prepare the command's process"; "cannot start bwrap"). A join
  into a group that is gone, or a deleted cwd, never reads as `prog: permission
  denied` ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).
- **The mark decides it**: a spawn error that the child's last pre-exec step
  did not precede is ral's
  ([[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]). `SpawnError` is
  `Prepare` or `Exec`, and `unforked` reads it: a `Prepare` failure is always
  ral's own (`unprepared`), whatever stands in the program's place; only an
  `Exec` failure depends on confinement. When a confining launcher runs in the
  program's place, its `Exec` failure is the launcher's, `Refused`.
- **A discarded child is killed and reaped** (`discarded`). A refusal after the
  fork, such as an unnameable payload group or a refused report, leaves no
  running child behind. `detach` leaves its survivor: it is never killed.

## Which failure reads as what

**The spawn's `errno` says what the OS found; the image says what that means.**
`at_spawn` reads it against the `Image` the launch was built from.

- **`ENOENT` for an image that still exists on the host is `not-runnable ...
  missing-interpreter`.** The file is there; what it needs is not. The failure
  names the `#!` interpreter read with the image (`interpreter_of`, kept in
  `Prepared`), never what the file says by the time the kernel refused. A bundled tool, or a file gone
  from its real path, is `not-found`.
- **Only a permission refusal is `permission-denied`.** `EACCES` at the spawn,
  or a path head's walk that fails by a refusal. Any other walk error at a path
  head reads as itself, `io` with the OS's words; absence and a non-directory
  component are `not-found`. At a `PATH` candidate every walk failure is absence.
- **A confined `EACCES` or `ENOENT` whose interpreter the launch's own
  projection hides is `permission-denied` plus the interpreter hint**, on both
  platforms: Seatbelt refuses the exec (`EACCES`), bwrap hides the interpreter
  (`ENOENT`), and the user sees one thing. `Prepared` keeps the launch's own
  projection, and only on a failure to start is the verdict asked of it
  (`interpreter_hint`): the exec table's, then the read table's, so an
  interpreter excluded, not admitted to run, or outside the read set each
  names its fix; with no projection, as inside the guest jail, there is
  no verdict and so no hint, and a hidden interpreter reads as missing.
- **The path-as-text hint is one rule, at the machine's `dispatch_exec`**
  (`offer_path_as_text`). It is offered only to a path head given no arguments,
  most likely meant as text, and is appended after whatever hint the grant's
  refusal already carries. It is offered to a refused admission and to a run
  that named a directory or plain file, never to another failure.

## The trampoline reports

**A confined launch's trampoline testifies on a report socket; the parent hears
the first word before it believes the program started.**
The records, the socket's discipline and why they hold are
[[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]].

- **Four records, each written once, in order**: `refused(message)`,
  `unstarted(errno)`, and on Linux `started` and `ended(status)`.
- **The first word.** `Testimony::first` returns once the program runs. On
  macOS the trampoline `execve`s in place, so its close-on-exec writer closing
  is the start, and EOF before a record says so. On Linux the trampoline
  supervises, so EOF before a record is an envelope that died: `Refused`.
- **The wait is a cancellable one**
  ([[invariants/a-first-word-is-waited-for-under-the-scope|a-first-word-is-waited-for-under-the-scope]]):
  a stop ends it, and the child is discarded.
- **What remains of the report on Linux rides the `ChildHandle`** as its
  `Testimony`, read at the reaper
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
  The supervisor that writes it is
  [[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]].

## Whose deny lines a failure owns

**A failure owns only the kernel deny lines its start claimed.** `start` takes
a `Claim` the moment the spawn returns, and `ending` hands `with_denials` the
`Failure` itself, never its rendering.

- **Confinement is `build_launch`'s fact**: a projection, or the guest jail.
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

**A child's `Group` is computed once, at its birth, and carried.** `Owns`
names a group of its own, signalled and killed whole; `Enveloped` its
envelope's payload's; `Joins` a pipeline's, addressed by its owner and
carrying the `Membership` it was placed by; `Shares` ral's own, where only its
pid addresses it. `placement` decides the policy once, `spawn_with_pgid`
returns the group with the child, an envelope's payload group becomes
`Enveloped`, and nothing downstream recomputes it
([[invariants/a-child-lands-once|a-child-lands-once]]). The process limit is a
property of the launch, applied inside each platform's spawn, so a child never
runs without the budget its grant states
([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

See also [[design/exec|exec]], [[design/failure|failure]],
[[internals/exec-judgment|exec-judgment]],
[[internals/confined-launch|confined-launch]],
[[internals/cancellation|cancellation]],
[[map/core/io-process|io-process]].
