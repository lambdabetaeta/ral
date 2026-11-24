# The ending is amended where it is reaped

**A confined child's reaped outcome is amended by its testimony in
`ChildHandle::into_watch`, before any consumer sees it.** Nothing downstream
corrects an ending: the collector, the standalone wait and `ending` all read an
outcome that is already what happened (`core/src/process/child.rs`,
`Testimony::amend` in `core/src/process/report.rs`).

- **On Linux the outcome is the supervisor's word.** A recorded `ended(status)`
  is the exact status: `Exited(code)` or `Signaled(n)`.
- **No record plus an exit of `128 + n` is `Signaled(n)`**, when `n` is a real
  signal. `Signal::of_status` is bounded to `1..=SIGRTMAX`, so `exit 200` of
  the envelope is an exit, not a death by signal 72.
- **Only the supervisor's own death leaves no record.** The supervisor writes
  `ended` before it exits, so a silent report under an envelope that exited
  `128 + n` is the supervisor killed by `n` (`SIGKILL` to the group is the
  case). Any other exit with no record stays as it is.
- **The report is read as final, without waiting.** The amendment runs on the
  reaper's thread, which must never block, so it drains what is buffered and
  reads that. Nothing more can come: bwrap is reaped only after its
  supervisor. Its monitor and pid 1 close their extra descriptors; the
  supervisor's copy closes in the kernel's `exit_files`, before
  `exit_notify`; a dying namespace init reaps every member before it is
  reaped itself; without a pid namespace the monitor waits for the supervisor.
  A writer still open then is a stray close-on-exec copy that a concurrent
  fork in ral took while the parent held its writer (`issue` to
  `Report::forked`), in a child not yet at its `execve`. It never writes, so
  it cannot hide a death (`a_stray_writer_cannot_hide_a_supervisors_death`).
- **macOS needs none.** The program replaced the trampoline, and its status is
  the child's own.

**Why.** An ending read in several places is decided in several places. One
amendment at the one door onto a wait, the reaper's, means a stop, a failure
and an audit record all describe the same death, and no consumer ever sees
bwrap's folded `128 + n` as an exit the program chose.

See [[invariants/a-supervisor-never-execs|a-supervisor-never-execs]],
[[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]
and [[internals/an-external-end-to-end|an-external-end-to-end]].
