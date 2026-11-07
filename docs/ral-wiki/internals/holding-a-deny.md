---
generated_at_commit: f1b0a4a4
verified_at_commit: f1b0a4a4
verified_at_date: 2026-11-07
anchors: [pinned_dirs, build_profile, DenyMask, Binds, Layer, Shown]
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

## Every enforcer pins the ancestors

**`path::pinned_dirs` (`core/src/path/render.rs`) names every proper ancestor
of a deny that lies within a write prefix**, the write prefix's root
included. `SandboxProjection::rendered` derives the set for every Unix
launch, and both Unix backends read it. A folded projection's pin slot is
`()` (`FsLists`'s `P`), so no pin reaches a backend but a derived one.

- **It covers every spelling.** The set is the ancestor closure of every
  *rendered* deny name within a rendered write name: the deny's surface
  spelling and its symlink-resolved target. So an alias's chain is pinned
  beside its target's, and a symlink swapped in after the sandbox starts is
  covered too.
- **Only a pinned directory's own name in its parent is frozen.** Its
  entries stay changeable.
- **The price is real, on every platform.** A grant that writes a repo and
  denies `.git/config` also refuses `mv .git .git.bak` and `rmdir .git`: both
  `.git` and the repo root are pinned ancestors of the denied entry.

### macOS refuses their unlink

- **`build_profile` (`core/src/authority/sandbox/macos.rs`) emits
  `(deny file-write-unlink (literal "<dir>"))` for each pinned directory**,
  after the write prefix's own allow, since Seatbelt's last match wins.
- **`literal`, never `subpath`.** That keeps a pinned directory's *entries*
  changeable.

See [[internals/seatbelt-profile|seatbelt-profile]] for the full rendering.

### Linux binds each onto itself

**Every launch binds each pinned directory, writable, onto its own name
(`Layer::Spine`, `core/src/authority/sandbox/linux/shown.rs`).** The bind mounts the
handle `Binds::open` holds, as every bind does.

- **A mountpoint cannot be moved.** `rename(2)` and `rmdir(2)` refuse it with
  `EBUSY`. The kernel asks this of the dentry in the envelope's mount
  namespace, so every name a bind shows the directory at is refused alike.
- **Its entries stay writable,** since the bind is.
- **An absent pinned name gets no bind:** bwrap would make its mountpoint on
  the host ([[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]).
- **A pinned name that now goes through a link refuses the launch,** as a
  linked allowance does. Bound, it would show the link's target on the
  deny's spine.
- **The pins go after the shown binds and before `/proc`, `/dev` and every
  mask.** A bind shows the host's tree, which holds none of the envelope's
  own mounts, so a pin laid later buries them: a pin of `p` laid after the
  mask at `p/secret` shows the secret. For the same reason a parent is pinned
  before its children; `pinned_dirs` is sorted.
- **Landlock sees nothing new.** A self-bind adds no inode to a path walk,
  so `Shown::roots` leaves the pins out.
- **Masks go on after every bind,** because the last mount wins. Each deny is
  masked at every name a bind shows it (`Binds::names`).

Measured on Linux 7.1.3 under bwrap 0.12.0: with `p` bound onto itself,
`mv p p2` and `rmdir p` fail with `EBUSY`, through a second bind's name too,
and a new file in `p` is written; with the pin laid after the mask, the
secret is read.

### Why the mask's own anchor is not enough

**A mask is a mount, and a mount follows its dentry, so renaming an ancestor
carries the mask along. That holds within one envelope only.** Every launch
renders its masks afresh, at the positions the deny holds.

- **A later grant loses the object.** After a confined `mv G/p G/p2` under a
  deny of `G/p/secret`, a grant decoded afterwards walks the deny's name,
  finds nothing at `G/p/secret`, and masks nothing. Its launches and ral's
  own reads then reach `G/p2/secret`.
- **A live hold follows the object** (`Hold::current`), but only while the
  grant that took it lives.
- **The pin also closes a race.** A concurrent confined command could swap
  an ancestor for a symlink between a launch's planning and bwrap's mount,
  and the mask would land at the link's target. A mountpoint cannot be
  swapped.

### Windows lists them only

**Carving grants each directory on the way to an exclusion only the listing
of its own names, with neither `DELETE` nor `FILE_DELETE_CHILD`**, so no
sandboxed program renames or removes one
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
The price is larger: nothing can be created in it either.

### ral's own builtins

**ral's `mv`, `rm` and `rmdir` are bundled tools, and a bundled tool runs as
a confined program like any other (`Image::Tool`).** The kernel's pin holds
them. The guard has no pin of its own, as
[[decisions/261021_one-path|one-path]] decided: under a grant, ral itself
renames only a staged file over the file a redirect writes, and removes only
what it made.

## The shape of the path picks the mount

**Choosing the wrong mount kills the launch, so `DenyMask::of` is the only
constructor, and it takes the shape `Deny::read` read off the denied path.** `--tmpfs` makes its
own mountpoint, so over an existing regular file it fails with `ENOTDIR` before
bwrap runs anything. That is a deny that denies nothing, because nothing runs.

- **An existing non-directory** takes `--ro-bind /dev/null`. It is bound
  without `MS_DEV`, so it cannot be opened either way.
- **A directory** takes `--perms 0000 --tmpfs`. The missing bits refuse the
  owner as much as anyone, so a tool that writes there is refused rather than
  told it succeeded. Nothing can enter it, so a command whose working
  directory lies at or beneath it is refused before bwrap runs.
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

**On Linux, a deny of a name absent at render is held by ral's own guard
alone. A spawned program can read a file the host creates there during the
launch, even under a read-only grant; under a writable prefix it can create
the name itself.** macOS holds it in full. The case is common: a grant that writes a
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
So `DenyMask::of` gives an absent name nothing (`LeftAbsent`).

- **Under a read-only bind the host can create the name during a launch,
  and that launch reads it; the next one masks it.** The bind refuses the
  child's writes, but the host still writes the underlying directory.
- **Under a writable bind the guard is the only enforcer.** It refuses ral's own
  reads and creation of the name, under every spelling
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
- **macOS holds both reads and creation.** Seatbelt's rules are negative and
  range over names, so a host-created file stays unreadable, and it refuses creation
  (`capabilities::seatbelt_holds_an_absent_deny_inside_a_write_prefix`).

### What a confined program can do

**Each launch whose render finds the name absent leaves it unmasked for that
launch's lifetime.** A *launch* is one external command ral spawns, with every
process that command starts. ral renders the envelope afresh for each launch,
even for two commands in the same grant body. Several overlapping launches can
therefore share the gap; a later render that finds the name present masks it.

- **During that launch, the program can read a file the host creates at the
  name**, even under a read-only grant.
- **Under a writable grant, the program can create the name** as a file, a
  directory or a symlink, and read and write what it made.
- **What it made stays on the host** after the launch ends.

### What it cannot do

- **ral cannot be made to read or create the name.** The guard refuses both,
  before and after the host creates it.
- **A launch rendered after creation cannot reach what was made.** It finds an object
  at the name and masks it as usual: a file under `/dev/null`, a directory
  under the empty tmpfs. Files created inside a planted directory are hidden
  with it.
- **A planted link is not the denied object.** What it reaches is judged on its
  own, by the guard and by every later launch, as SPEC §12.4 says.

### What is at stake

**The risk includes a leaked secret and a planted file.** A host writer can
create `.env` after the launch starts, and that launch can read its secrets
even under a read-only grant. Under a writable grant, a child can also leave
content at a name the grant promised no confined program would write. Two
kinds of reader may trust that planted content:

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

`docs/SPEC.md` §12.4 states the user-facing rule.

## Tests

**The failures here are a sandbox that never launches and a mount that lies.
Neither shows up in an argv assertion, so each rule is pinned by a test that
spawns the envelope for real:**

- `authority::sandbox::linux::tests::spawn::a_denied_path_refuses_every_access_while_the_body_still_runs`
- `authority::sandbox::linux::tests::spawn::building_an_envelope_never_creates_a_denied_name_on_the_host`
- `authority::sandbox::linux::tests::pin::a_denys_spine_cannot_be_moved_and_its_entries_stay_writable`
- `authority::sandbox::linux::tests::pin::rals_own_mv_and_rm_cannot_move_a_denys_spine`
- `capabilities::deny_pin_survives_ancestor_rename` (`ral/tests/capabilities.rs`,
  Linux and macOS): through ral itself, the rename is refused and the denied
  file stays at its name.
- `capabilities::seatbelt_holds_an_absent_deny_inside_a_write_prefix` (macOS)

- `deny_absent::a_host_created_absent_deny_is_visible_only_to_the_running_linux_child`
  (`ral/tests/deny_absent.rs`, Linux and macOS): a pipe holds the child while
  the host creates `.env`; Linux reads it only in that launch, macOS never
  reads it, and ral's own read is refused before and after creation.

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** A denied object
renamed or hard-linked under an allowed directory is hidden from ral's own
readers by identity and visible to a program ral runs, since the bwrap mask,
the Seatbelt rule and the DACL carve are placed at the resolved real paths
and judge names ([[design/two-enforcers|two-enforcers]]). Seatbelt's pinned
ancestors are `RealPath::ancestors_within` of the positions each exclusion
holds.

The Unix renderers place an exclusion at both positions it holds, its name's
and its link's target's. The Windows DACL side places it at one, the target
where the name is a link (`Walked::followed`, as `carve` reads it): it judged the followed
path alone before, carving never grants a link entry anyway, and
`ensure_unreachable` holds a position with every link refused, which the
link's own position would fail.
