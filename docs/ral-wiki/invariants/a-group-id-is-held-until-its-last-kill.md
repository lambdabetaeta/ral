# A group id is held until its last kill

**A pgid that `kill(-pgid)` may still address is never freed for reuse: one
pin, the process whose pid is the group's id, is kept as a zombie until the
collector's last kill of the group.** A pid stays allocated while its zombie is
unreaped, and a pgid equals its leader's pid, so holding the zombie holds the
name.

- A pipeline's pin is its anchor. `end_anchor` only ends its witness; the
  anchor stays a zombie, held by its `Watch`, until `PipelineGroup` drops,
  after the fold and every pump-join kill in it. `PipeNode`'s field order makes
  a collector dropped by an unwind kill while the anchor is still held.
- A lone child that leads the group its collector owns is its own pin
  (`CollectState.pin`, an `Arc<Watch>` shared with its stage). Filing its end
  does not reap it (`StageHandle::file_external_end` reaps through
  `Arc::try_unwrap`, which fails while the pin holds a reference); `settle_lone`
  drops the pin after `drained`, once no kill addresses the group.
- A returned failed start has no collector pin to inherit. `Launch::spawn`'s
  `discarded` kills the owned group **before** killing/reaping the child, while
  the leader still pins its ID. A refused terminal handoff retains saved state
  through this teardown, then restores and releases the jail. A failed joined
  launch leaves the borrowed group to its enclosing owner. `StartFailure`
  retains the drains until caller settlement; a pipeline rolls back while its
  anchor still pins the group, even if no stage registered, then joins before
  settling failed-start drains. That final settlement has an empty kill
  callback: it retains no numeric group ID for a kill after reap.
- Any other filed external is reaped as it files: it has left `stages`, so no
  kill names its pid.
- Behind an envelope there is no pin: the payload leads the group, and the child
  the collector holds is only the envelope's monitor, whose pid is not the
  group's id.

Why: the reaper's `WNOWAIT` leaves the zombie to its `Watch`, so reaping is a
decision of the collector, taken last; a reaped leader's group, once empty,
lets the kernel hand its id to an unrelated process, and the next group kill
would land on it.

See [[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]]
and [[internals/pipeline-execution|pipeline-execution]].
