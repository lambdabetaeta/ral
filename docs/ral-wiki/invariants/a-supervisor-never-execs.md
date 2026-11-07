# A supervisor never execs

**The Linux trampoline is a supervisor, so it never becomes anything: it
spawns the program, or forks the bundled tool, waits, and reports.** A process
that execs has no later, and the report's last record, `ended`, is written
after the program's end. The code is
`core/src/authority/sandbox/warrant/trampoline.rs` (`Confined::run`, one per
platform; `start_file`, `start_tool` and `supervise`). This page is the one home
of the supervisor and of the `SIGSEGV` example below.

- **Before the spawn**, `outlive_group_signals` gives each of `SIGINT`,
  `SIGTERM`, `SIGHUP` and `SIGQUIT` a no-op handler, so the supervisor survives
  a signal sent to the payload's whole group and can still report. A signal
  that was inherited as ignored (`SIG_IGN`) is kept ignored: the program
  inherits the user's choice, not ral's. A caught disposition resets to the
  default at the program's `execve`.
- **The program dies with its supervisor.** The spawned program's pre-exec, and
  the forked tool child, each set `PR_SET_PDEATHSIG(SIGKILL)` and then check
  `getppid` against the supervisor's pid (`die_with`): a supervisor already
  gone before the `prctl` leaves no death signal to hear, and the check turns
  that into a failure instead of an orphan. The failure is ral's own: the child
  tells it on a close-on-exec pipe, and the supervisor reports `refused`, never
  `unstarted`, which is the file's `execve` alone
  ([[invariants/a-launchs-report-is-the-trampolines-alone|the report's records]]).
- **The supervisor itself sets no death signal.** Its life is tied to ral's by
  the envelope: a launch the session keeps passes bwrap `--die-with-parent`
  (with `--info-fd`), so the envelope, and the supervisor inside it, ends when
  ral does. A surrendered (`detach`) launch carries neither, deliberately: the
  survivor must outlive ral. The supervisor outlives every signal sent to the
  payload's group but `SIGKILL`.
- **`SIGPIPE` is ignored before the supervisor writes**, so a vanished reader
  (a detached launch whose parent is gone) is a failed write, never a death.
  A write that fails finds nobody to tell and is dropped. The report's end is
  typed to match: `Witness::started` yields a `Running`, whose `ended` is the
  only way to write `ended`, so no path says an end before a start.
- **It reports the exact wait status**, `ended`, and exits with the program's
  code, or `128 + n` for a death by signal `n`. A confined `SIGSEGV` is
  therefore `signaled 11` and an `exit 139` is `exited 139`.
- **A bundled tool is forked, not run in-process.** A tool may itself `execve`
  (`env`), which would close the report untold. The forked tool child begins as
  a spawned program does: it resets the caught dispositions to their defaults,
  clears the signal mask, sets `PDEATHSIG`, and has closed its copy of the
  report (`tool_child`); the tool's own entry, `uutils::serve`, restores
  `SIGPIPE`. It is silent, and tells the supervisor whether it began, one
  word on a close-on-exec pipe, before it runs the tool: the supervisor says
  `started` only after that word, and a child that cannot begin is `refused`
  with its errno, never a tool that `exited 126`. It never returns into the
  supervisor's frames, by a panic either.
- **The trampoline is single-threaded up to that fork.** Its warrant role starts
  no thread, so the forked child may run anything. Landlock's `restrict_self`
  is called without `TSYNC` and so binds only the calling thread; it confines
  the whole process only because no second thread exists.

**Why.** Under the envelope, bwrap folds the payload's death into an exit
status, `128 + n` for a signal, which cannot tell `signaled 11` from `exit 139`.
Only a process that waits for the program can say what happened to it, and it
can say so only if it outlives the program, outlives the group's signals, and
cannot be inherited by it. macOS keeps `execve` in place: there the program
keeps the trampoline's pid and its wait status is exact. The choice is
[[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]].

See [[internals/confined-launch|confined-launch]],
[[internals/linux-envelope|linux-envelope]] and
[[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]].
