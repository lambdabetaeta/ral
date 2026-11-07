# A process limit binds before the child runs

**A grant's process budget is a property of the `Launch`, applied inside each
platform's spawn, so a child under a grant never runs without it.**
`build_launch` (`core/src/shell/exec/command/process.rs`) sets it behind one
gate, `has_active_capabilities`, by `Launch::limit_resources`
(`core/src/process/limits.rs`); nothing is applied to the child afterward.

- **Unix applies it in `pre_exec`.** The hooks run in registration order: the
  session's `ChildLimits` soft ceilings first, then the grant's, which set soft
  and hard together and so win. On macOS the budget is a fork limit measured in
  the parent, in the child's `RLIMIT_NPROC`; a budget that cannot be measured
  refuses the launch.
- **Windows applies it in the suspended job assignment, under the groups
  lock.** The limit is set on the Job Object (a pipeline member's, or one made
  for the child alone) before the child, created suspended, enters it and is
  resumed, by `set_active_process_limit`, which holds `GROUPS` across its read-modify-write
  of the job's one `LimitFlags` word. A write outside the lock could strip
  `KILL_ON_JOB_CLOSE` off a group's job, or re-arm one that `disown` had just
  cleared.
- **A budget that cannot be set refuses the spawn** (`unbudgeted`), as a
  `Refusal`: the alternative is a command that runs with no budget while saying
  it has one.

**Why.** A limit applied after the spawn leaves a window in which the child
runs uncapped, and it needs a second code path per platform to close it, each
with its own failure to handle. A limit that is part of the launch is applied
where the child is born, by the same code that makes it, and a failure to apply
it is the launch's failure.

See [[design/two-enforcers|two-enforcers]],
[[invariants/a-child-lands-once|a-child-lands-once]] and
[[internals/an-external-end-to-end|an-external-end-to-end]].
