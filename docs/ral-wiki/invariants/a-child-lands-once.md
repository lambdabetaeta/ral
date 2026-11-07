# A child lands once

**Where a child landed is computed once, when it is born, and carried as a
value.** `Landing` is `Owns(Pgid)`, `Joins` or `Shares`
(`core/src/process/group.rs`), and `Spawned.landing` hands it from the spawn to
every later reader (`core/src/process/launch.rs`).

- **`Owns(pgid)`**: a group of its own, or its envelope's payload's. It is
  signalled and killed whole.
- **`Joins`**: a pipeline's group, addressed as a whole by its owner, through
  the stage's `Membership`.
- **`Shares`**: ral's own group, where only the child's pid addresses it.
- **`spawn_with_pgid` is the one derivation**
  (`core/src/process/group/unix.rs`): it returns the landing with the child. A
  `Join` policy joins; `NewLeader` and `NewSession` own the child's own group;
  `Inherit` shares. An enveloped launch is placed `NewLeader` whatever was
  asked, and its payload's group, named once it exists, replaces the landing
  (`Owns`); Windows' spawn returns its own.

**Why.** Who may signal what is the safety of every teardown: a kill aimed at a
group the child does not lead hits a stranger, and one aimed at a pid the child
shares misses its tree. A landing recomputed in several places, from the policy
asked for, the leader answered and the envelope's shape, is several chances to
disagree; a value carried from the one place that knows cannot be recomputed
wrongly.

See [[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]],
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/cancellation|cancellation]].
