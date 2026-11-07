# A process group is its owner's to signal

**A collector signals or kills a process group only if it owns it; every other
child is addressed by pid.** Ownership is where the child *landed*: `Group` is
`Owns(Pgid)`, `Enveloped(Pgid)`, `Joins(Membership)` or `Shares`
(`core/src/process/group.rs`), and a collector reads its authority from nothing
else. This page is the one home of what each landing means. That a landing is
decided once is [[invariants/a-child-lands-once|a-child-lands-once]].

- **`Owns(pgid)`**: a group of the collector's own, led by the child (a lone
  command) or by the pipeline's anchor. The collector holds the group and
  signals and kills it whole.
- **`Enveloped(pgid)`**: a confining envelope's payload's group, which the
  envelope's session keeps apart from every group ral addresses. It is signalled
  and killed whole, as an owned one is, and is what a claim reads to know the
  launch ran behind an envelope.
- **`Joins(membership)`**: a stage's group, as a nested pipeline joins it. The
  group is the stage owner's: the collector addresses its externals by pid, and
  `Group::owes`, through `Membership::owes`, decides what its collector may
  signal: a cause only when it is stronger than the stage scope already holds.
  The `Membership` rides the `PgidPolicy::Join` the child was placed by.
- **`Shares`**: ral's own group. The child is addressed by pid, never with
  `kill(-pgid)`, and gets Ctrl-C once, the terminal's, and never again by pid:
  `owes` is every cause but `Interrupted`.

**Why.** A group has one owner who delivers every cause the group holds to all
of it at once, so a second delivery from a member is a second interrupt, and a
signal to a group one does not own reaches processes outside one's scope. ral's
own group contains ral, so a `kill(-pgid)` there would end the shell itself.

Held by `Group::owned`, `Group::owes` and `CollectState::addresses`, which
build every address from it (`core/src/process/group.rs`,
`core/src/shell/exec/pipeline/collect.rs`). `addresses` yields the owned group
if any; else each external by pid; and, beside either, every envelope's payload
group, which no signal to the collector's group reaches. A collector keeps the
`Group` it was handed, so an owned child is never signalled twice, once as a
group and once by pid.

See [[invariants/a-group-id-is-held-until-its-last-kill|a-group-id-is-held-until-its-last-kill]],
[[internals/pipeline-execution|pipeline-execution]],
[[internals/cancellation|cancellation]] and
[[design/pipelines|pipelines]].
