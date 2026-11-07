# A teardown kills before it joins

**A teardown kills what it launched before it joins anything that can block,
and a collector dropped mid-flight joins nothing.** The order, in
`CollectState::cancel_all`:

1. the cause is written on every live stage;
2. the grace signal, once per address (`grace_signal`; none for `ReaderGone` and
   `Aborted`), a group's followed by `SIGCONT`;
3. the grace is waited out in slices, ending at once under `Aborted`;
4. the group, or the pid, *and* the guest jail's cgroup are killed (`kill_live`);
5. only then the drain, and, in settlement, the jail finished and the pumps
   joined (`drained`).

- A descendant that outlives its stage holds a pump's pipe open, and no join
  returns while it does; the kill comes first so that it can.
- `CollectState::drop` kills and never joins: a pump a dropped stage holds is
  detached, as is one whose stage was cut for a dead write.
- The jail's cgroup is killed with the group, since a member that `setsid()`'d
  leaves the group but not the cgroup, and finished before the pump join.
- On Windows the group is one owned `Job`: `Job::drain` waits for it to empty
  and kills it once under a cause, and its `Drop` releases it.

See [[invariants/the-terminal-returns-before-the-pumps|the-terminal-returns-before-the-pumps]],
[[internals/pipeline-execution|pipeline-execution]] and
[[internals/cancellation|cancellation]].
