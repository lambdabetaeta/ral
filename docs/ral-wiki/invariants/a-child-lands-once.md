# A child lands once

**Where a child landed is decided once, before it is born, and carried as a
value.** `spawn_with_pgid` (`core/src/process/group/unix.rs`) returns the
child's `Group` with the child, and `Spawned.group` hands it from the spawn to
every later reader (`core/src/process/launch.rs`). The same type is what a
collector holds: nothing converts between a landing and a group, and nothing
recomputes one. What each landing allows a collector to signal is
[[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]].

- **Placement is one decision.** `command::placement` yields a `Placement
  { policy, loan, readable }` for a lone command: its group, the terminal
  prepared for it, and whether the terminal may be its stdin. A pipeline's
  direct stage takes `command::member_policy`, the same rule for a joined
  group. An envelope leads a group of its own, so both say `NewLeader` for an
  enveloped launch, and `Launch::spawn` asserts it rather than overriding.
- **`spawn_with_pgid` is the one derivation.** A `Join` policy joins, and
  carries its membership into `Joins`; `NewLeader` and `NewSession` own the
  child's own group; `Inherit` shares. An enveloped launch's payload group,
  named once it exists, becomes `Enveloped` (the envelope's own group where it
  names none); Windows' spawn returns its own.
- **A launch is spent by its spawn.** `Launch::spawn` and `spawn_detached`
  take the launch by value, so a second spawn is unwritable. The spawn moves
  the confinement out of the launch, so a second spawn on the same `Command`
  would find none, and would read bwrap's exit as the program's; the `pre_exec`
  hooks, likewise, cannot be applied twice.
- **A failed handoff of the terminal kills the child.** A lone `NewLeader`
  child is spawned before its group can be lent the terminal; if `tcsetpgrp`
  then fails it sits outside the foreground, where its first tty read stops it
  for ever. It is killed and settled, and the refusal is the command's verdict
  (`unlent`). A pipeline lends before any stage exists, so its stages read
  `holds_terminal` after the lend and never meet this.

**Why.** Who may signal what is the safety of every teardown: a kill aimed at a
group the child does not lead hits a stranger, and one aimed at a pid the child
shares misses its tree. A landing recomputed in several places, from the policy
asked for, the leader answered and the envelope's shape, is several chances to
disagree; a value carried from the one place that knows cannot be recomputed
wrongly.

See [[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]],
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/cancellation|cancellation]].
