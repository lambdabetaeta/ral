# A deny is read once

**On Linux each fs deny is read from the disk once, when the launch is planned,
and every consumer takes that one answer.** This page is the one home of
that rule. `Shown::of`
(`core/src/authority/sandbox/linux/shown.rs`) reads each deny with `Deny::read` into a
`Deny { name, mask, position }`, sorted by name; nothing after the plan asks the
disk what stands at a denied name.

- **bwrap renders the plan.** `bwrap_argv` walks the planned `Binds::denies` and
  emits each `Deny::mask` at every name a bind shows, reading nothing for it.
- **Landlock reads the same plan.** `landlock::build` takes `&Shown` and keeps
  its exec ruleset out of `Shown::scratch`: the positions of denies planned as
  `DenyMask::EmptyDir`, the directories the envelope covers with a tmpfs.
- **One owner, one call.** `enveloped` (`authority/sandbox/launch.rs`) plans once
  and hands the same `Shown` to both. Under an unrestricted fs there is no deny
  to read.
- **Pinned by** `linux::tests::render::bwrap_and_landlock_take_each_denys_mask_from_one_read`.

**Why.** The mask follows the kind of the thing denied (`DenyMask::of`): a
directory gets an empty `0000` tmpfs (`EmptyDir`), a file or device node a
read-only bind of `/dev/null` (`UnopenableNode`), a symlink nothing (`OnItsTarget`:
its resolved twin holds it), and an absent name nothing (`LeftAbsent`: a mount
there would have bwrap create the name it forbids). A tmpfs is the sandboxed
user's own, so the child may `chmod` it open and write there. Landlock must
therefore admit nothing from above a directory the envelope masks that way. Were
the two to read the disk separately, a deny that changed kind between the reads,
a file replaced by a directory, could be masked as a directory by bwrap while
Landlock, having read a file, admitted the path from above: a tmpfs the user may
`chmod`, under an `Execute` admission the mask was meant to withhold. One read
makes the two agree by construction, not by timing.

See [[internals/linux-envelope|linux-envelope]],
[[internals/holding-a-deny|holding-a-deny]],
[[design/two-enforcers|two-enforcers]].
