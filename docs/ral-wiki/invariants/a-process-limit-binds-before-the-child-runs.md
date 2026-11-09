# A process limit binds before the child runs

**A grant's process budget is a property of the `Launch`, applied inside each
platform's spawn, so a child under a grant never runs without it.**
`Command::prepare` (`core/src/shell/exec/command.rs`) sets it behind
one gate, `has_active_capabilities`, by `Launch::limit_resources`
(`core/src/process/limits.rs`); nothing is applied to the child afterward.

- **macOS applies it in `pre_exec`.** The launch's one hook
  (`ChildSetup::install`) applies the session's `ChildLimits` soft ceilings
  first and the grant's after them, whatever order they were configured in;
  the grant's set soft and hard together and so win. The budget is a fork
  limit measured in the parent, in the child's `RLIMIT_NPROC` (Darwin compares
  a process count with the *forking* process's own limit, so it refuses the
  child's descendants and no other process of the user's); a budget that
  cannot be measured refuses the launch at build.
- **Linux sets no process budget.** `limit_resources` there turns core dumps
  off and nothing else; the process budget is the other platforms'.
- **Windows applies it in the suspended job assignment, under the groups
  lock.** The limit is set on the Job Object (a pipeline member's, or one made
  for the child alone) before the child, created suspended, enters it and is
  resumed, by `set_active_process_limit`, which holds `GROUPS` across its
  read-modify-write of the job's one `LimitFlags` word. A write outside the
  lock could strip `KILL_ON_JOB_CLOSE` off a group's job.
- **A budget that cannot be set refuses the launch** (`unbudgeted`, an `Error`
  of `unavailable`'s wording: "this command would run with no process budget"):
  on macOS at build, on Windows at the spawn, as `Unstarted::Refused`. The
  alternative is a command that runs with no budget while saying it has one.

**Why.** A limit applied after the spawn leaves a window in which the child
runs uncapped, and it needs a second code path per platform to close it, each
with its own failure to handle. A limit that is part of the launch is applied
where the child is born, by the same code that makes it, and a failure to apply
it is the launch's failure.

See [[design/two-enforcers|two-enforcers]],
[[invariants/a-child-lands-once|a-child-lands-once]] and
[[internals/an-external-end-to-end|an-external-end-to-end]].
