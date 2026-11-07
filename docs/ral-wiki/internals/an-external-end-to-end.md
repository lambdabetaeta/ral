---
generated_at_commit: b9f3d1b708751d86428c306362f82a72d2904928
verified_at_commit: b9f3d1b708751d86428c306362f82a72d2904928
verified_at_date: 2026-11-07
anchors: [prepare, start, start_detached, unstarted_error, ending, Started, Unstarted, Spawned, Landing, SpawnError, spawn_with_pgid_after, attributed, unforked, unprepared, vanished, Confined, Testimony, Witness, Record, at_spawn, offer_path_as_text, interpreter_of, interpreter_hint, supervise, into_watch]
---

# An external, end to end

**An external's life is one path in three named stages, and each stage raises
its failures one way.** The path is `core/src/shell/exec/command/process.rs`;
the process layer under it is `core/src/process/launch.rs`. A plain command, a
directly launched pipeline stage and `detach` each wire their own streams
between the first two stages and share nothing else. What the operation means
is [[design/exec|exec]]; what a confined launch adds is
[[internals/confined-launch|confined-launch]].

## Three stages

| stage | function | it does | its failures leave as |
|---|---|---|---|
| prepare | `prepare` | reads the head's verdict, vets the arguments (they are words), builds the launch (`Image`, the `#!` interpreter, the projection, env, cwd, limits), reads the run's scope once more | the head's verdict; a `Refusal` as raised text; a stop as the run's stop |
| start | `start`, `start_detached` | take the `Prepared` by value and spawn it, returning `Started { shown, args, born }`; the one conversion of `Unstarted` | `unstarted_error`, below |
| ending | `ending` | a death by a signal of a cause in force is that cause's stop; any other end is `Error::process`, with the kernel's denial hint where a sandbox ran the child | a stop, or a process failure |

- **Every failure of a stage has one constructor.** `From<Refusal> for Break`
  words every refusal of `prepare`; `unstarted_error` is the only place a launch
  that made no child becomes the run's error; `ending` is the only place a
  child's end does. A fourth way to raise is a defect.
- **`prepare` reads the scope on the way out.** Confinement can run for minutes
  on Windows, so a wall that expired during it is found before the child
  exists, not after.
- **Where a child ended is amended before `ending` sees it**
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).

## How a start fails

**`Unstarted` says whose failure it was, and the answer is never inferred from
an `errno`.** `Launch::spawn` and `spawn_detached` return nothing else.

| case | it is | it arises from |
|---|---|---|
| `Program(io::Error)` | the program's own failure to become a process | the `execve` of an unconfined launch; the trampoline's `unstarted(errno)`; on Windows only `CreateProcessW`'s own error |
| `Refused` | never the program's fault | a launcher or envelope that fails to exec; a payload process group that cannot be named; the trampoline's `refused`; a Linux envelope that died before reporting; a step of ral's own before the `execve`, confined or not (on Windows also a `.bat` or `.cmd`, a NUL in an argument, handle admission) |
| `Stopped` | the run's stop, while waiting for the trampoline's first word (Unix only) | `Testimony::first` |

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
  (`interpreter_hint`); with no projection, as inside the guest jail, there is
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

## Where the child landed

**A child's `Landing` is computed once, at its birth, and carried.** `Owns`
names a group of its own (or its envelope's payload's), signalled and killed
whole; `Joins` a pipeline's, addressed by its owner; `Shares` ral's own, where
only its pid addresses it. `spawn_with_pgid` returns it with the child, an
envelope's payload group replaces it, and nothing downstream recomputes it
([[invariants/a-child-lands-once|a-child-lands-once]]). The process limit is a
property of the launch, applied inside each platform's spawn, so a child never
runs without the budget its grant states
([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

See also [[design/exec|exec]], [[design/failure|failure]],
[[internals/exec-judgment|exec-judgment]],
[[internals/confined-launch|confined-launch]],
[[internals/cancellation|cancellation]],
[[map/core/io-process|io-process]].
