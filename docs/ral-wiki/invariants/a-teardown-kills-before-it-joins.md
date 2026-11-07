# A teardown kills before it joins

**A teardown kills what it launched before it joins anything that can block.**
A collector that is dropped mid-flight kills and joins no pump; the thread
stages it still holds are cancelled and joined, but only after the kill. The
order is that of `CollectState::cancel_all` (`core/src/shell/exec/pipeline/collect.rs`):

1. the cause is written on every live stage (`escalate`, monotone: a weaker
   cause is already covered);
2. the grace signal, once per address, where the cause has one (`grace_signal`:
   none for `ReaderGone` and `Aborted`), and the grace is waited out in slices,
   ending at once under `Aborted`;
3. unless the cause is `Aborted`, a live thread stage in a shielded cleanup is
   waited for up to the cleanup grace;
4. every address is killed, the group or the pid, and each live stage's guest
   jail cgroup (`kill_live`);
5. only then the drain of the remaining events and, in settlement, the jail
   finished and the pumps joined (`drained`).

- **Why the kill is first.** A descendant that outlives its stage holds a
  pump's pipe open, and no join returns while it does. Filing a stage's end
  therefore joins nothing; the fold joins, after the kill.
- **A drop kills and joins no pump.** `CollectState::drop` writes `Terminated`
  on every live stage in one sweep and kills; a pump a dropped stage holds is
  detached, as is one whose stage was cut for a dead write. The one join a drop
  does perform is `ThreadStage::drop`'s: each thread stage never observed is
  cancelled and joined, a slice at a time, sharing the strike the collector has
  already made, so the join waits on a stage whose children are dead.
- **The jail dies with the group.** A member that `setsid()`'d leaves the group
  but not its cgroup, so the cgroup is killed beside the group and finished
  before the pump join.
- **A stop names an owner only when the scope struck it.** The verdict a
  teardown leaves, where no stage halted, takes the owner of the cause in force
  on the collector's scope only when that cause equals the teardown's; a
  teardown the stages themselves began, by an `exit` or a panic, has none
  (`CollectState::fold`).
- **Windows.** The group is one owned `Job`: `Job::drain` waits for it to empty
  and kills it once under a cause, and its `Drop` releases it.

The terminal is returned before the pumps are joined
([[invariants/the-terminal-returns-before-the-pumps|the-terminal-returns-before-the-pumps]]).
See also [[internals/pipeline-execution|pipeline-execution]] and
[[internals/cancellation|cancellation]].
