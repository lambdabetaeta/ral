# A process group is its owner's to signal

**A collector signals or kills a process group only if it owns it; every other
child is addressed by pid.** Ownership is where the child *landed*
(`Group`, from the `PgidPolicy` it was spawned under), and the collector
reads it from nothing else:

- `Owns`: a group of one, led by the child, with no anchor; `Enveloped`, its
  envelope's payload's. The collector holds the group and signals and kills it
  whole.
- `Joins`: the stage's group, as a nested pipeline joins. The group is the
  stage owner's: the collector addresses its externals by pid, and
  `Group::owes`, through `Membership::owes`, decides what its collector may
  signal: a cause only when it is stronger than the stage scope already holds.
- `Shares`: ral's own group (`Group::Shares`). The child is addressed by pid,
  never with `kill(-pgid)`, and gets Ctrl-C once, the terminal's, and never
  again by pid: `owes` is every cause but `Interrupted`.

Why: a group has one owner who delivers every cause the group holds to all of
it at once, so a second delivery from a member is a second interrupt, and a
signal to a group one does not own reaches processes outside one's scope. ral's
own group contains ral, so a `kill(-pgid)` there would end the shell itself.

Held by `Group::owned`, `Group::owes` and `CollectState::addresses`, which
build every address from it (`core/src/process/group.rs`,
`core/src/shell/exec/pipeline/collect.rs`).

See [[invariants/a-child-lands-once|a-child-lands-once]],
[[internals/pipeline-execution|pipeline-execution]],
[[internals/cancellation|cancellation]] and
[[design/pipelines|pipelines]].
