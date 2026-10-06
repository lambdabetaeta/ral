---
status: active
generated_at_commit: 9bdbb945
verified_at_commit: 78d13526
anchors: [Binds, Bind, render_objects, bwrap_argv, bwrap_command, HostEnvelope, Slot, open_real, Pinned]
---

# The envelope mounts by handle

**Every object the bwrap envelope shows is opened by ral, in the host, never
through a symlink, and mounted by that descriptor (`--ro-bind-fd`,
`--bind-fd`); bwrap is never handed a source name.** What the child sees at a
granted name is the object the grant rendered there, whatever a same-uid writer
does to the name between the render and the mount.

## Context

bwrap's `--ro-bind X X` resolves `X` in the host when it mounts, following
symlinks. Between `SandboxProjection::rendered` and the mount, a same-uid
writer, a concurrent confined child included, could `mv X moved; ln -s
elsewhere X`, and the envelope showed `elsewhere` at `X`: measured, by name the
child sees the redirect, by handle the original. The grant judged one object
and the kernel mounted another.

A second gap rode along. An exec grant's directory was never mounted, so
`exec: ['~/.cargo/bin/': allow]` ran nothing from it unless `fs:` also named
it; macOS, where an exec allow is readable, did not have that gap.

## Decision

`Binds::open` (`core/src/sandbox/linux.rs`) is the one place a launch opens
what it mounts. It plans every bind the projection asks for, opens each object
once with `openat2(O_PATH | O_CLOEXEC, RESOLVE_NO_SYMLINKS)`, and lends one
copy per bind at `Slot::Mount(i)`, the open-ended run after the fixed slots
(fd `107 + i`), bwrap closing each after the one mount it serves. `bwrap_argv`
renders the same vector, so the argv and the handoff are two readings of one
table, and its signature holds no descriptor.

- **Objects and spellings.** A rendered path is its canonical spelling, the
  object, and the names that reach it (`render_objects`, the grouping
  `render_paths` flattens). The object is mounted at its own name and at each
  spelling, `/bin` on a merged-`/usr` host showing `/usr/bin`'s handle, unless
  the spelling lies within another bind, where the host already shows its own
  symlink and bwrap would refuse the mount.
- **Coverage.** A read-only bind within another bind is dropped, and a
  writable one only within another writable one: a read-only mount inside a
  writable prefix would deny writes the grant admits.
- **What is shown.** The system's read-only defaults, the read and write
  prefixes, the program's image (`Program::File`'s real path, the file the
  trampoline execs; nothing at the spelling, which nothing inside looks up),
  every allowed file of the exec table and every live allowed directory, as on
  macOS. Over everything, the cgroup tree and the two pinned binaries.
- **What is frozen.** Between the shown binds and `/proc`, read-only, each
  live allowed directory a write prefix covers without naming, so the writable
  prefix around it still takes writes and a veto carried into it cannot be
  authored around; under `fs: Unrestricted` with a veto, the whole admitted
  set, files included, over `--dev-bind / /`
  ([[decisions/261006_a-veto-freezes-what-a-write-covers|a-veto-freezes-what-a-write-covers]]).
  Each is frozen wherever a writable bind shows it: a prefix written
  through a symlink (`write: ['/home/u']` with `/home -> /var/home`) is two
  binds of one handle, and a mount laid inside one does not show at the
  other, so a freeze laid only at the canonical name would leave the admit
  writable through the spelling. The freeze and the Landlock walk read one
  classification, `landlock::write_reach`.
- **Absent and raced.** `ENOENT` and `ENOTDIR` drop a bind: the open decides,
  with no window between a check and the mount. `EACCES` drops a system
  default no grant names (`/home/linuxbrew` owned by another user), and
  refuses a prefix the grant names. A name that now reaches another object
  refuses the launch ("`X` now reaches `Y`, not what the grant rendered"), and
  so does an open that meets a symlink (`open_real`: "`X` now goes through a
  symbolic link that ral will not follow; reload the grant or restart ral if
  you made that change"), exec paths having been frozen when the grant was
  decoded, possibly long before.
- **Fail closed on an old bwrap.** `HostEnvelope::builds` probes the
  mechanism itself: a bare envelope first, then one lending `/` at
  `Slot::Mount(0)` through the real `Handoff`, each running ral's own pin by
  descriptor as a pipeline anchor, never a host binary that may be absent. A
  bwrap that builds no envelope refuses every confined launch in its own
  words; one without `--ro-bind-fd` (before 0.8.0) refuses it naming its path
  and the version. There is no by-name fallback: it would
  keep the race alive on exactly the hosts least likely to be updated, and be
  a second renderer to keep in agreement.

Still by name, with no source to open: the tmpfs mounts, `--proc`, `--dev`
and its by-hand fallback, `--dev-bind / /` under `fs: Unrestricted`, and the
deny masks, whose only source is `/dev/null`. A mask goes on at every name a
shown bind gives the denied path, for the same reason the freeze does. A deny
that hides `/proc` is refused when the argv is rendered: masks go on after
`--proc`, and bwrap execs the trampoline through it.

### The trampoline by descriptor, and the locked files

bwrap's payload is `/proc/self/fd/102 --warrant`: ral's boot pin is lent at
`Slot::Trampoline`, and the kernel execs the inode behind it in every envelope
shape, under a tmpfs root and under `--dev-bind / /`, with or without a pid
namespace. A private path for the trampoline was the alternative, and dies
under `--dev-bind / /`, its parents `mkdir`ed on the host root. So the
trampoline is the boot inode by construction, and Linux no longer re-stats the
name before a launch (`Pinned::verify` is macOS's alone, where the exec is by
name): after a replace-by-rename it runs unlinked, under the parent that pinned
it, which is the consistent outcome.

The same pins lock the files. `Binds::open` lends each pin, bwrap's and
ral's, as a read-only `Over` bind by its own descriptor (a copy, one fd
serving one mount) at the name its inode holds now (`Pinned::names`, a
`readlink` of the pin), so a write there is `EROFS`, a hard link `EXDEV`, and a
rename or unlink of the mountpoint `EBUSY`; renaming an ancestor carries the
mount along. Both enforcers are frozen the same way, and no ancestor lock is
needed: nothing on the warrant path execs ral by name. An unlinked pin has no
name to lock, ` (deleted)` being no destination, so it gets no bind, and runs
by descriptor all the same.

## Consequences

- bwrap verifies `fstat(fd)` against `lstat(dest)` after each mount, so a
  destination the host turned into a symlink after the open kills the launch
  rather than moving the mount.
- **The destination residual.** A destination is still a name bwrap resolves
  inside the new root. A writer that turns an *ancestor* under a writable bind
  into a symlink during setup can put a handle's content at another name, or
  make bwrap `mkdir` on the host, the class
  [[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]
  already names; it cannot expose an inode no handle holds.
- **Judge to open.** The image is opened at the real path the guard judged,
  so the window in which a renamed ancestor substitutes another program
  narrows from judge→`execve` to judge→open; closing it wants `Program` to
  carry a handle from `Head::resolve`.
- An exec-allowed directory outside every `fs:` prefix is readable inside.
- **The locked-file residual.** A second hard link to a pinned inode
  elsewhere on the host, or another mount of its filesystem, is a writable
  name the bind does not cover. It is stated rather than warned about per
  launch.
- **The pins are the one open that follows symlinks.** Every object a
  launch mounts or admits is opened `O_PATH` without following a symlink; the
  pins are `O_RDONLY` and follow them, being taken at boot from `current_exe()`
  and the `PATH` ral started with, before any shell or grant exists, so what
  they follow is the host's install and never a child's write.
- **The bind-mount residual.** The Landlock walk judges what it reaches by
  inode, as with hard links: a host bind mount of a denied directory beneath
  an allowed one is admitted there. One looping back onto a directory being
  listed ends the walk at the repeat (`(dev, ino)` along the spine).
- **An unlinked trampoline needs a filesystem that serves it.** On virtiofs
  the exec of an unlinked pin fails, so after a swap the launch dies in
  bwrap's `execve` rather than asking for a restart. bwrap cannot mount an
  unlinked file at all, so a filesystem that hides an `unlink(2)` from an open
  descriptor's link count (FUSE, as rootless podman's fuse-overlayfs) gets the
  bind and a dead launch ("Can't find source path"); a replace-by-rename is
  seen there, and runs.
- The child must never `stat` `current_exe()`: inside, it reads the host
  spelling of the pin, ` (deleted)` after a swap.
- A host whose locked `/etc/hosts` refuses a read-only rebind (a rootless
  container) refuses it by descriptor too: no `Restricted` envelope launches
  there, as before.
