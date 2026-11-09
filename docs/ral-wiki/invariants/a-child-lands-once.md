# A child lands once

**Where a child landed is decided once, before it is born, and carried as a
value.** `spawn_with_pgid` (`core/src/process/group/unix.rs`) returns the
child's `Group` with the child, and `Spawned.group` hands it from the spawn to
every later reader (`core/src/process/launch.rs`). The same type is what a
collector holds: nothing converts between a landing and a group, and nothing
recomputes one. What each landing allows a collector to signal is
[[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]].

- **Requested placement is one decision.** `Shell::group_owner` supplies stage
  membership or session permission and interactivity. `Placement::command`
  selects `Join`, `Tenant`, `Inherit` or `NewSession`; `Placement::input`
  determines terminal stdin from the request. A pipeline selects its input
  once from its anchor handoff result and stores it for every stage.
- **The spawn owns envelope placement.** `Launch::spawn` steps an envelope
  `NewLeader` whatever was requested. A requested `Tenant` retains readable
  stdin and an opaque restoration-only hold, not a handoff or key attribution.
- **`spawn_with_pgid` is the one derivation.** `Join` carries its membership
  into `Joins`; `NewLeader`, `NewSession` and direct `Tenant` own the child's
  group; `Inherit` shares. An envelope's payload group, named once it exists,
  becomes `Enveloped` (the envelope's own group where it names none); Windows'
  spawn returns its own. `Tenant` lowers only to the fresh-leader step, but
  `Pgid` itself proves positivity only: freshness and same-session placement
  remain handoff call-site preconditions, not typed provenance guarantees.
- **A launch is spent by its spawn.** `Launch::spawn` and `spawn_detached`
  take the launch by value, so a second spawn is unwritable. The spawn moves
  the confinement out of the launch, so a second spawn on the same `Command`
  would find none, and would read bwrap's exit as the program's; the `pre_exec`
  hooks, likewise, cannot be applied twice.
- **A failed command handoff retains its guard through teardown.**
  `Launch::spawn` kills the owned group while its leader still pins the ID,
  kills/reaps the child, restores saved state, then releases the jail.
  `Prepared::start` returns a `StartFailure` owning its reason and drains,
  naming the handoff and OS error without claiming the command never ran.
  The caller settles before reporting; a pipeline first rolls back and joins
  so a joined child's descendants cannot block owner teardown. Settlement
  never kills by group ID after reap. A pipeline instead lends before any stage exists:
  its mode-inert anchor needs no teardown before restoration on refusal, and
  the pipeline proceeds with terminal input withheld.

**Why.** Who may signal what is the safety of every teardown: a kill aimed at a
group the child does not lead hits a stranger, and one aimed at a pid the child
shares misses its tree. A landing recomputed in several places, from the policy
asked for, the leader answered and the envelope's shape, is several chances to
disagree; a value carried from the one place that knows cannot be recomputed
wrongly.

See [[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]],
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/cancellation|cancellation]].
