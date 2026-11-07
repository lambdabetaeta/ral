# Ral's failure is never the program's

**`Unstarted::Program` is the program's own failure to become a process and
nothing else; everything that is not its fault is `Refused`, and a stop is
`Stopped`.** The type has three cases (`core/src/process/launch.rs`), and each
case is a claim about blame that the run's error then repeats to the user.
This page is the one home of that claim.

- **`Program(io::Error)`** is the `execve`'s own error, whether the parent's
  (an unconfined launch) or the trampoline's `unstarted(errno)` (a confined
  one). It becomes the process failure the head's probe would have reported:
  `not-found`, or `not-runnable` with a reason, the image deciding what `ENOENT`
  means: a file still at its real path whose interpreter or loader is missing is
  `missing-interpreter`, a vanished file or a bundled tool is `not-found`.
- **`Refused(Error)`** is never the program's fault, and its text is ral's own.
  It covers a launcher or envelope that fails to exec, a payload process group
  that cannot be named, the trampoline's `refused`, an envelope that died
  before reporting, and a step of ral's own that failed before the `execve`
  (placement, signal reset, limits, the guest jail, stdio, `detach`'s fork, and
  on Linux the trampoline's own fork or `die_with` before the program begins),
  confined or not (`unprepared`: "ral could not prepare the command's process").
  Only a failure of the `execve` itself depends on confinement: where a
  launcher was to run in the program's place, its failure to exec is
  `Refused`.
- **On Windows only `CreateProcessW`'s own error is `Program`**; a `.bat` or
  `.cmd`, a NUL in an argument, handle admission and a Job Object that will not
  take the child are `Refused`.
- **`Stopped`** is a cancellation the start sees: before the child is made,
  before one of Windows' stamps, or while waiting for the trampoline's first
  word. It is the run's stop, not a failure. Building a launch only describes
  it, so it is never stopped there
  ([[design/exec#Modes|exec]]).

**One conversion.** `started` (`core/src/shell/exec/command.rs`) is the one
place an `Unstarted` becomes the run's error: `Refused` and `Stopped` pass
through, and `Program` goes through `at_spawn`, which also adds, under an
active sandbox, the hint that a script's `#!` interpreter is withheld.

**Why.** An error blames a subject: `prog: permission denied` accuses `prog`.
A launch that fails in ral's own step, a join into a process group that is gone
or a working directory deleted under it, would accuse the one program that was
never started, and a host without `bwrap` would read as a grant that denies
everything. The cases are therefore fixed by *where* the failure arose, not by
the `errno` it carries: the same `EACCES` is the program's at its `execve` and
ral's at its `setpgid`. No `io::Error::other(String)` crosses this boundary;
text that is ral's own is an `Error` built where the failure is known
(`unavailable` for a host that cannot confine or budget a launch, `unprepared`
for a step that failed).

How the parent tells the two apart is by the *mark*: for an unconfined launch
[[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]], for a confined one the
trampoline's own report
([[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]).
See also [[internals/an-external-end-to-end|an-external-end-to-end]] and
[[design/exec|exec]].
