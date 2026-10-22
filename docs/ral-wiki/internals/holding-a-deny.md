---
verified_at_commit: 002c654a
verified_at_date: 2026-10-13
anchors: [pinned_dirs, build_profile, DenyMask, Binds]
---

# Holding a deny

**A `deny` must survive the filesystem moving around it, not just name a path to
refuse.** [[design/grant|grant]] states the invariant: no confined child can
make a denied path's contents reachable under a name the deny does not cover.
This page says how each platform keeps it, and names the one case Linux
cannot keep. It is the deny half of
[[internals/capability-enforcement|capability-enforcement]].

## The threat is an ancestor

**The denied entry itself is safe. Its parent is not.** Writing, unlinking or
hard-linking a `deny_paths` entry is already refused. But an *ancestor* of that
entry lies outside the deny and inside the write prefix's allow. A confined `mv`
or `rm` could move the ancestor, and carry the denied bytes to a name nothing
covers.

## macOS pins the ancestors

**`FsRules::pinned_dirs` (`core/src/sandbox/projection.rs`) collects every
proper ancestor of a deny that lies within a write prefix**, the write prefix's
root included.

- **It covers every spelling.** The set is the ancestor closure of every
  *rendered* deny name within a rendered write name: the deny's surface
  spelling and its symlink-resolved target. So an alias's chain is pinned
  beside its target's, and a symlink swapped in after the sandbox starts is
  covered too.
- **`build_profile` (`core/src/sandbox/macos.rs`) emits
  `(deny file-write-unlink (literal "<dir>"))` for each pinned directory**,
  after the write prefix's own allow, since Seatbelt's last match wins.
- **`literal`, never `subpath`.** That keeps a pinned directory's *entries*
  changeable. Only its own name in its parent is frozen.
- **The price is real.** A grant that writes a repo and denies `.git/config`
  also refuses `mv .git .git.bak` and `rmdir .git`: both `.git` and the repo
  root are pinned ancestors of the denied entry.

See [[internals/seatbelt-profile|seatbelt-profile]] for the full rendering.

## Linux needs no pin

**On Linux a deny is a mount, and a mount is anchored to the inode it covers,
not to the path that named it** (`DenyMask`, `core/src/sandbox/linux.rs`).

- **Renaming an ancestor carries the mask along.** It keeps covering the real
  file at its new place.
- **Renaming or removing the mountpoint itself fails** with `EBUSY`.
- **So the invariant holds by construction, and the price is smaller than on
  macOS.** Only the denied path's own name is frozen. Every ancestor, the write
  prefix's root included, stays free to rename and remove. `pinned_dirs` goes
  unused.
- **Masks go on after every bind,** because the last mount wins. Each deny is
  masked at every name a bind shows it (`Binds::names`).

## The shape of the path picks the mount

**Choosing the wrong mount kills the launch, so `DenyMask::over` is the only
constructor, and it reads the shape of the denied path.** `--tmpfs` makes its
own mountpoint, so over an existing regular file it fails with `ENOTDIR` before
bwrap runs anything. That is a deny that denies nothing, because nothing runs.

- **An existing non-directory** takes `--ro-bind /dev/null`. It is bound
  without `MS_DEV`, so it cannot be opened either way.
- **A directory** takes `--perms 0000 --tmpfs`. The missing bits refuse the
  owner as much as anyone, so a tool that writes there is refused rather than
  told it succeeded.
- **A symlink** gets no mount, since nothing mounts over a symlink. The deny is
  masked at the resolved target, which `SandboxProjection` carries beside the
  surface spelling.
- **A name that does not exist** gets no mount at all. See the next section.

**The directory mask has a stated limit**
([[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]).
The tmpfs belongs to the sandboxed uid. A child that deliberately `chmod`s the
bits back gets private scratch space at that name: memory the host never sees,
and never the denied directory. Only a read-only mount would make the mode
fixed. bwrap gives that only for a bind, and a bind needs an empty source
directory this process holds for the whole run. Whether the mask is worth that
is open.

**A Linux mask refuses with `EACCES`; macOS refuses with `EPERM`.** A
cross-platform test should assert that the bytes cannot be reached, not which
errno came back.

## A name that does not exist yet

**On Linux, a deny of a name that does not exist, inside a
writable prefix, is held by ral's own guard alone. A spawned program can create
that name.** macOS holds it in full. The case is common: a grant that writes a
whole repo and denies `.env`, run on a fresh checkout with no `.env`.

### Why there is no mask

**Every mount bwrap could lay needs a mountpoint, and for an absent name bwrap
would have to create one.** The binds are identity binds of real host
directories, so that creation lands on the host disk:

- **under a read-only bind** it fails with `EROFS`, and the whole envelope dies
  before the command runs;
- **under a writable bind** it succeeds, and the deny creates the very name it
  forbids, on the host, for good.

Neither is a deny ([[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]).
So `DenyMask::over` gives an absent name nothing (`LeftAbsent`).

- **Under a read-only bind nothing is lost.** Creating the name is the only
  access an absent name has, and the read-only bind already refuses it.
- **Under a writable bind the guard is the only enforcer.** It refuses ral's own
  creation of the name, under every spelling
  ([[internals/path-judgment|path-judgment]]). It cannot see what a spawned
  program does.
- **Windows carves the grant instead, and holds it.** A Windows sandbox cannot
  have access taken away beneath a folder it is granted, absent name or
  present: the `AppContainer` access check ignores a deny ACE naming a
  capability. So an allowance holding an exclusion is granted as its existing
  entries less the excluded ones, and each directory on the way only its own
  listing, by an ACE nothing inherits. A name made there during the launch,
  the excluded one included, inherits nothing granted and stays unreadable;
  the cost is that nothing can be created in a carved directory at all
  ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]],
  amendment of 2026-10-20).
- **macOS does not.** Seatbelt's rules are negative and range over names, not
  objects, so it refuses the creation
  (`capabilities::seatbelt_holds_an_absent_deny_inside_a_write_prefix`).

### What a confined program can do

**The gap lasts for one launch.** A *launch* is one external command ral spawns,
with every process that command starts. ral renders the envelope afresh for each
launch, even for two commands in the same grant body.

- **During that launch, the program can create the name** as a file, a
  directory or a symlink, and read and write what it made.
- **What it made stays on the host** after the launch ends.

### What it cannot do

- **ral cannot be made to create the name.** The guard refuses it.
- **No later launch can reach what was made.** The next launch finds an object
  at the name and masks it as usual: a file under `/dev/null`, a directory
  under the empty tmpfs. Files created inside a planted directory are hidden
  with it.
- **A planted link is not the denied object.** What it reaches is judged on its
  own, by the guard and by every later launch, as SPEC §12.4 says.

### What is at stake

**The risk is a planted file, not a leaked secret.** There was no secret at the
name to leak. The harm is content sitting at a name the grant promised no
confined program would write. Two kinds of reader may trust it:

- **another process in the same launch**: for example a test runner started by
  the same `sh -c` that wrote the file;
- **anything outside the grant, later**: the user's own tools, or a later
  session that runs without this grant.

### Why nothing closes it

**On Linux, no mechanism refuses a child the creation of exactly one name
without touching the host or refusing more than that name.** Each candidate
fails for a stated reason:

- **A bwrap mount at the name** must create its mountpoint on the host first.
  If the host later removes that name, the mask quietly unmounts.
- **A tmpfs over the parent directory** hides the parent's other entries.
- **An overlay over the writable prefix** sends the child's writes to an upper
  layer instead of the host directory. A grant to write the directory would no
  longer write it.
- **Creating the name, masking it, and removing it afterwards** writes to the
  host, which an envelope must never do.
- **Landlock** only allows, and its create rights attach to a directory and
  cover every name in it. To refuse `.env`, it must refuse every new file in
  that directory.
- **A seccomp filter** sees a pointer to the path, never the path itself.
- **A seccomp user-notification supervisor** can read the path from the child's
  memory, but another thread of the child can change it between the check and
  the use. The kernel's documentation warns against using it to enforce
  security policy.
- **fanotify permission events** need privilege, and none of them refuses a
  creation.

`docs/SPEC.md` §12.3 states the user-facing rule.

## Tests

**The failures here are a sandbox that never launches and a mount that lies.
Neither shows up in an argv assertion, so each rule is pinned by a test that
spawns the envelope for real:**

- `sandbox::linux::tests::a_denied_path_refuses_every_access_while_the_body_still_runs`
- `sandbox::linux::tests::building_an_envelope_never_creates_a_denied_name_on_the_host`
- `capabilities::seatbelt_holds_an_absent_deny_inside_a_write_prefix` (macOS)

**No Linux test pins the absent-name gap itself.** `tests/unix/grant-fs-deny.ral`
denies only names that already exist.

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** A denied object
renamed or hard-linked under an allowed directory is hidden from ral's own
readers by identity and visible to a program ral runs, since the bwrap mask,
the Seatbelt rule and the DACL carve are placed at the resolved real paths
and judge names ([[design/two-enforcers|two-enforcers]]). Seatbelt's pinned
ancestors are `RealPath::ancestors_within` of the positions each exclusion
holds.
