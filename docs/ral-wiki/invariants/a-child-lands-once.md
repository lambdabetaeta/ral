# A child lands once

**Where a child landed is decided once, before it is born, and carried as a
value.** `Group` is `Owns(Pgid)`, `Enveloped(Pgid)`, `Joins(Membership)` or
`Shares` (`core/src/process/group.rs`), and `Spawned.group` hands it from the
spawn to every later reader (`core/src/process/launch.rs`). The same type is
what a collector holds: nothing converts between a landing and a group.

- **`Owns(pgid)`**: a group of its own, signalled and killed whole.
- **`Enveloped(pgid)`**: a confining envelope's payload's group, which the
  envelope's session keeps apart from every group ral addresses; signalled and
  killed whole. It is what a claim reads to know the launch ran behind an
  envelope.
- **`Joins(membership)`**: a pipeline's group, addressed as a whole by its
  owner. The `Membership` rides the `PgidPolicy::Join` the child was placed
  by, so the landing carries the stage scope it answers to.
- **`Shares`**: ral's own group, where only the child's pid addresses it.
- **Placement is one decision.** `command::placement` yields a `Placement
  { policy, loan, readable }` for a lone command: its group, the terminal
  prepared for it, and whether the terminal may be its stdin. A pipeline's
  direct stage takes `command::member_policy`, the same rule for a joined
  group. An envelope leads a group of its own, so both say `NewLeader` for an
  enveloped launch, and `Launch::spawn` asserts it rather than overriding.
- **`spawn_with_pgid` is the one derivation**
  (`core/src/process/group/unix.rs`): it returns the group with the child. A
  `Join` policy joins, and carries its membership into `Joins`; `NewLeader`
  and `NewSession` own the child's own group; `Inherit` shares. An enveloped
  launch's payload group, named once it exists, becomes `Enveloped`; Windows'
  spawn returns its own.
- **A launch is spent by its spawn.** `Launch::spawn` and `spawn_detached`
  take the launch by value, so a second spawn is unwritable and the confinement
  and `pre_exec` hooks cannot be applied twice.
- **A failed handoff of the terminal kills the child.** A lone `NewLeader`
  child is spawned before its group can be lent the terminal; if `tcsetpgrp`
  then fails it sits outside the foreground, where its first tty read stops it
  for ever. It is killed and settled, and the refusal is the command's verdict.
  A pipeline lends before any stage exists, so its stages read `holds_terminal`
  after the lend and never meet this.

**Why.** Who may signal what is the safety of every teardown: a kill aimed at a
group the child does not lead hits a stranger, and one aimed at a pid the child
shares misses its tree. A landing recomputed in several places, from the policy
asked for, the leader answered and the envelope's shape, is several chances to
disagree; a value carried from the one place that knows cannot be recomputed
wrongly.

See [[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]],
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/cancellation|cancellation]].
