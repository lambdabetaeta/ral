---
verified_at_commit: 6281520c
generated_at_commit: 6281520c
verified_at_date: 2026-10-27
anchors: [walk, walk_excluded, Trace, RealPath, Located, Dir, Hold, Excluded, Listed, ObjectId, Unjudged, Judged, Installing, InstallToken, GrantStack::judge, Refused, Extent, from_capabilities, rehold, LexicalPath::folded, collision_key, is_discard, check_device_name, admits_fs, Baseline, Walk, Cancellation, AdmittedWalk, Shell::admitted_walk]
---

# Path judgment: what ral checks when it touches a file

**A name is never authority. Every name becomes an object in one place, the
walk; the walk writes down what it opened and hands back the handles; a
judgment consumes that transcript; the action that follows goes through the
handles.** Policy is resolved to objects once, when a layer is decoded, and
never again ([[decisions/261021_one-path|one-path]], which argues why).
This page is the in-process half of
[[internals/capability-enforcement|capability-enforcement]] for the filesystem.
The code is `core/src/path/` and `core/src/authority/`: `authority::grant`
resolves authored layers, `authority::policy` judges the resolved model, and
`authority::guard` enforces each in-process check.

## The transcript

**The walk descends from the root through directory handles and splices every
link itself.** Each open is relative to a handle it already holds, so a swap
between steps changes which child is obtained and the transcript says which.

- **A `Trace` is the evidence.** The root's identity; one `Step` per component
  opened, each with the name as the filesystem stores it, the `ObjectId` and the
  kind, all read from the handle just opened; and the *missing tail*, empty for
  an object, one name for a create target. Its `RealPath` is computed from the
  steps by the one constructor, so it cannot disagree with them. It also names,
  as `passed`, each link whose target the walk read and each directory a `..`
  climbed out of, by position: what else a lookup of the spelling looks up,
  which the Seatbelt profile lets it stat.
- **A `RealPath` is a position**: no link, `.` or `..` in it. A `Trace` is a
  position and the proof of having walked to it. Both are plain values; a
  `Trace` is made by the walk, or read off the same-host wire by a
  `Deserialize` that checks the shape of the whole.
- **A link is read through the handle held on it** (`readlinkat(fd, "")`,
  `freadlink`, the reparse buffer) and its target walked from the link's own
  directory, so no name is read twice. A `..` climbs out of the link's target,
  not out of the link's place, and pops the cursor rather than opening the
  kernel's parent. From a held directory — a link the tree walk follows, an
  ignore file found beneath it — a `..` climbs the directories the walk opened
  above that directory, never the names a caller descended by, and at the root
  stays.
- **The stored name is the one the host can prove.** Linux: the name `openat`
  resolved. macOS: the name asked where the lookup answers that spelling;
  else, for an object the walk holds, the one entry of the parent, among those
  that fold as the name does, whose lookup reaches the held identity (a lookup,
  not the listing, crosses a mount point or a firmlink as the step did); an
  object the walk only states, under another spelling, is refused. Windows:
  the normalized name of the handle, checked against the parent's own path. On
  Windows the walked prefix cannot be renamed while held, because directories
  are opened with the traverse right and without `FILE_SHARE_DELETE`; every
  create is relative to the held handle.
- **Anchoring is not folding.** A logical consumer (`cd`, `within [dir: …]`,
  `absolute-path`, a display base) folds `.` and `..` explicitly
  (`LexicalPath::folded`); what reaches the disk is handed unfolded
  ([[decisions/261021_access-is-physical|access-is-physical]]).
- **A listing makes a `Listed`, not a `Trace`**: what a listing saw of an
  entry, judged only as a name shown. Names are enumerated first; each entry's
  metadata and identity are read when yielded. On Windows that is an
  attribute-only, non-following open relative to the held directory, not the
  enumeration's cached size or identity: a removed entry is absent, a replaced
  one is judged as its new object, and an open writer's flushed size is current.

## Three holders, and who vouched

**Possession is separate from evidence, and the handle's type says who
vouched for it.**

- **`Dir`** is an open directory; **`Located`** an object named exactly: its
  directory's handle, its trace, and the leaf's handle where the leaf exists;
  **`Hold`** an exclusion's anchor: a trace and, on Unix, a handle on the
  deepest object walked, kept for the lifetime of the stack holding it. A `Hold`
  serialises as its trace and deserialises *unheld*, which is a sound state.
- **`Dir` and `Located` are `#[must_use]`, have no `Clone`**, and the
  operations that end an object's use take it by value (`stage`, `create_dir`,
  `launch`, `open_write`). Nothing holds a handle longer than the operation
  it was walked for.
- **The marker is the voucher.** `Unjudged` is what the walk returns and offers
  its trace and nothing more. `Judged<Reading>` and `Judged<Writing>` are minted
  by `GrantStack::judge` and nothing else. `Installing` is minted by the two
  `RealPath::open_*_nofollow` against an `InstallToken` only `authority::sandbox` can
  make. Bytes come out of a judged handle only, and go in through a write-judged
  one only; a stat or a link's target needs a judged or installing one. A
  descent keeps its authority (`Installing` to `Installing`; `Judged` to
  `Unjudged`, which the region judges again). So `walk(p)?.read()` does not
  compile, in this crate or another.

The door is `Shell::locate` (and `locate_dir`, `locate_existing`,
`locate_if_admitted`, `judge`): walk, then `GrantStack::judge` on the transcript,
then the handle. These shell adapters live in `shell/runtime/state/checks.rs`:
`authority::guard` returns diagnostic data, and the adapter records a denial
and raises its runtime error. Every write is a `locate`.

**A failed walk's diagnostic is judged before it is disclosed.** A spent link
budget retains the transcript ending at the actual link, without the unresolved
continuation. `Shell::walk_failure` judges that evidence for the requested
operation: a refusal records a denied check, while admission preserves the loop
error, including for probes. This authorizes disclosure only, never an operation
through an unresolved name. Missing or unenterable tails use the lenient-name
judgment. Successful accesses still judge the final object, not each hop.

## Two readings

**One method per polarity decides whether a rule covers an access.**

- **`RealPath::contains`, an allowance**: the access's position lies at or
  within the rule's, component by component, names as stored. Position is
  transitive, which the `Table` meet needs; a directory removed and recreated
  under a grant keeps working.
- **`Excluded::matches`, an exclusion**: the same position reading under
  `collision_key` (the slot, whatever occupies it); *or* an identity of the
  rule (a step's, never the root's) appears on the access's trace and the rest
  of the rule's name continues the access's past it (the object, wherever it is
  reached from and whatever it is called, and its name under a renamed
  ancestor). Every occurrence is tried, since a bind mount can place one
  directory on a trace twice. `MatchedBy` records which way matched.

The two readings are two methods on two types, so an allowance cannot hold by
identity: there is no code for it. An identity's only effect is negative, so a
reused one over-denies and never admits.

## Resolving

**A layer is resolved where it is decoded; the fold of a stack is a pure
function of its layers.**

- **`shell::grant::decode_capability_map` walks each spelling as it freezes it**
  through `authority::grant`, runs the physical refusals (`xdg` within home,
  `refuse_the_world`, the toolchain floor) on that trace, and keeps it. A `Capabilities` that arrives already decoded is
  resolved by `GrantLayer::from_capabilities`, the same walk. A `GrantLayer`
  keeps its rules and nothing beside them; each spelling is a `GrantPath`:
  as written, and what its walk made of it.
- **An allowance is an `Extent`**: `Everywhere` where the spelling is, or walks
  to, the root; otherwise a position, with what its walk passed on the way. It
  holds nothing and resolves anywhere — on every host; the Windows sandbox side
  refuses to project it at launch.
- **An exclusion is an `Excluded`, made by `walk_excluded`**: the spelling walked
  without following its last component; where that is a link, its target read
  through the link's handle and walked on. Both are held: the link is an object
  a probe can see and a name that can be retargeted, its target what a write
  would reach. A link retargeted later is still the denied link, and its new
  target is judged on its own.
- **A bare `` `name `` exec key is resolved on the `PATH` of the decoding
  environment** and kept the same way; a name not found keeps its veto.
- **Resolving can fail**: a walk that fails for any reason other than an absent
  or unenterable tail refuses the layer, naming the spelling. A directory the
  walk cannot enter ends the spelling's identities there; the rest is position,
  folded lexically. Where a `..` folds that name away, the walk resumes from
  the directory it holds, so `missing/../x` is walked to `x`, its link
  followed by the walk's mode, as though `missing` had never been spelled.
- **Across a process boundary** a stack travels as its traces and the receiver
  calls `GrantStack::rehold` (the seed's decode, `run_framed`): each exclusion is
  walked again at its position and held where the object there is the one
  recorded; otherwise its identities stand unheld. Nothing is refused. A stack
  that is not `root()` is dispatched to a local engine only.

## Use through the handle

**What was judged is what is touched.**

- A `Located` opens, stats, stages and publishes relative to its directory
  handle; a `Dir` lists and descends from its own. A data handle on a present
  leaf is derived from the held one (`/proc/self/fd/N` on Linux, an empty-name
  `NtCreateFile` relative to the held handle on Windows). The native Windows
  open also supports directories; `ReOpenFile` forces a non-directory open.
  macOS cannot derive one: it opens the leaf by name inside the
  directory handle and verifies it against the held identity before any byte
  moves.
- **A leaf absent at the walk is created exclusively.** What was judged was an
  absence; a file that has appeared since was not judged, so the write fails
  with `AlreadyExists` rather than truncating it.
- **A write is staged beside the leaf and published under the held directory.**
  A present leaf is staged over only if `may_write` admits it: `>` writes the
  file, staging only makes the write atomic, and the rename alone would ask the
  directory. The after-image is read from the staging handle: what ral wrote,
  never what landed.
  Windows publishes with `NtSetInformationFile(FileRenameInformationEx)`, the
  staging handle as source and the held directory as `RootDirectory`. The
  Win32 setter does not preserve this relative-destination form.
- **A name ral invents** (`temp-dir`, `temp-file`) is an absent child of a
  write-judged directory, judged as that leaf and created exclusively.
- **ral enumerates.** `path/tree/walk.rs` lists with `Dir::read_dir`, judges every
  directory it enters and every file it reads for its own guidance against a
  `ReadRegion`, and uses `ignore` for its matcher alone. Its `Entries` yield
  domain `Cancellation`s (cause and owner), and `WalkEntry::name` returns a
  string refusal; neither depends on runtime. `Shell::admitted_walk`
  (`shell/runtime/walk.rs`) supplies the live region, scope and naming base.
  Its `AdmittedWalk` skips unseen entries, converts cancellation to a
  non-catchable stop, and gives naming errors their caller's context.

## Exec

**A program is judged as a `Located`, its `#!` line read through that handle,
and it is started by its real name on every platform.** The `Located` has one
owner at a time: `Head::admit` moves it from the `Head` into the `Admitted`;
`launch` takes the `Admitted` and drops the `Located`, so no judged program or
directory handle survives into the run. Landlock rules only allow and cannot
require the judged object, and a handle kept through the run would hold the
program's directory on Windows ([[internals/exec-judgment|exec-judgment]]).

## The OS sandbox side

**The renderers consume frozen allowances and the positions of held exclusions.**
bwrap and Landlock open allowance positions without following links, as
`Installing`. An absent allowance is omitted; a redirected one refuses launch.
Filesystem rendering takes exclusions' positions once per launch for bwrap's
masks and Seatbelt's denies and pins. Seatbelt also renders exec exclusions at
those positions.

Landlock judges enumerated entries and exact-file allowances by identity as
well as position. It lists a directory where an exclusion's recorded position
lies inside it or the directory is on that exclusion's recorded spine by
identity, so renaming an ancestor preserves its listing. A program denied
only from running, moved into an allowed directory on no exclusion's recorded
spine and holding no recorded position, runs.

Carrier discovery keeps its system-only walk's handle, judges its trace against
exclusions on both axes, then reads through that handle. Linkers meet the same
predicate. Refused-exec hints use the interpreter carried in the parent's
warrant.

### A hold's current position and lifetime

**A live hold survives `rehold`, even after rename or unlink.** An unheld hold,
such as one deserialized from the wire, is reacquired only where its recorded
position still reaches its recorded identity. The stack holds the descriptor
for its lifetime: the agent session or the grant block.

`Hold::current` asks Linux's `/proc/self/fd` or macOS's `F_GETPATH` for the
held object's name, walks it without following the leaf, and keeps it only
where the walk reaches the held identity. It appends the trace's missing tail:
a moved directory holding an absent `.env` yields the new `.env` position,
never the whole directory. This verifies Linux's ambiguous ` (deleted)` suffix
without parsing it. An unheld or deleted object has no current position;
Windows holds nothing and returns none. `Excluded::positions` deduplicates each
hold's recorded and current positions. Hard links, copies, wire transmission
and moves after launch retain the limits in
[[design/two-enforcers|two-enforcers]].

Windows carving opens each listed entry without following the leaf and judges
its trace by `Excluded::matches`. It retains the examined objects: unlisted
allowances in `whole`, granted entries in `held`, and listed directories in
`entries`. `FrozenPin` derives stamping rights from these handles; only unlisted
allowances and the program image require a fresh walk. What carving examined is
what receives the ACE, even if the name is replaced before stamping.

## A deny holds under every spelling

**One file can have several names. A deny holds under all of them; an allow
holds as stored**
([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]).

- **On macOS** the walk spells each component as the directory stores it, so a
  case-variant meets a deny. **On Windows** an 8.3 alias meets a deny recorded
  under its long name.
- **For a name that does not exist**, stored spellings cannot help; the position
  reading under `collision_key` does. Creating `.ENV` meets an absent
  `` deny: [`cwd .env] ``, and the refusal says another spelling caught it.

## Exceptions decided before any region

**One target needs no authority, and one write no grant can admit.**

- **The discard device is always allowed** (`LexicalPath::is_discard`:
  `/dev/null`, `\\.\NUL`); the same predicate decides whether the act counts as a
  fact ([[design/audit|audit]]). `GrantStack::admits_fs` does not share the
  exemption: it decides membership of a region.
- **A DOS device name is refused, not excused** (`authority::guard::check_device_name`), before any
  region is consulted.
- **A write onto a pinned binary is refused**: `Refused::Guarded`, for the binary
  the sandbox pinned at boot, judged by inode and *before* the stack is folded,
  since a stack with no `fs` opinion is `Unrestricted`
  ([[internals/linux-envelope|linux-envelope]]). Replacing it by rename stays
  allowed.

## Soundness, and what stays by name

**ral judges each object as it stood when the walk entered it.** A directory
already entered and then renamed by another process is judged at its old
position; the kernel judges current ancestry. The residuals:

- **A program is exec'd by name.** A file put at its real name, or at a folder on
  the way, between the judgment and the exec runs unjudged. A Landlock file rule
  admits the inode opened and judged at install time.
- **The publish names its source on Unix**: a process that may write the
  directory can swap the staged sibling in the instant between sync and rename.
  On Windows the rename is by the staging handle.
- **macOS opens a held leaf by name** for data before verifying it; a device
  substituted at the name is opened with `O_NOCTTY` and, for a regular file,
  `O_NONBLOCK`. A leaf its permissions refused, and a special file (FIFO,
  socket, device), is *stated* (one `getattrlistat`) and never held; a refused
  file is never opened, a special file is opened by name under the held
  directory for a stream, which must find the object the walk stated there, by
  identity, and still a special file, before a byte moves (unheld, its
  identity can be reused, and the kind refuses a regular file that came to
  bear it); a substitute is opened first, as at a held leaf, and a
  substituted FIFO with no peer blocks that open. The step looks an
  entry up before it opens it, and no open refuses a special file by kind: a
  process that may rename in the directory can put a FIFO or a device at the
  name in between, and the event-only open is then that FIFO's reader, or runs
  that device's open routine, for the instant before the identity check
  refuses it — during the walk, before any judgment.
- **A file moved or hard-linked out of a denied directory** by a process that may
  write it is a new name, judged on its own. A secret that must follow its
  moves is denied by its own name.
- **Identity is imperfect on some volumes.** Linux holds the exclusion's object
  with `O_PATH`, so its id is not reused and `umount` answers `EBUSY`. macOS holds
  it with `O_EVTONLY`, which an unmount revokes; Windows holds nothing, and NTFS
  and ReFS ids are not reused in practice. Ancestors are recorded, not held.
  FUSE, NFS, overlayfs without `xino` and FAT can reuse or fail to keep ids; an
  exclusion there can over-deny and position still holds. No volume refuses an
  exclusion. FAT answers neither `FileIdInfo` nor a reparse tag; the 64-bit
  index and a tag of zero stand in.
- **A held directory keeps its ancestry open.** A tree walk holds one handle
  per directory above where it stands; a followed absolute link opens its
  target's ancestry afresh, so nested absolute links multiply the handles held,
  and a walk can run out of descriptors. A link the walk could not follow for
  any reason but that it leads nowhere, to a non-directory or round a loop is
  reported as not entered, never shown as a plain link with its subtree
  dropped.
- **Probes by name**: `may_write` (the `is-writable` builtin, and a staged
  write's check of the file it replaces), the `+x` test of a `PATH` candidate,
  the diagnostic probes after a failed exec, and git's session-start reading of
  `.git` answer by name and grant nothing. A casefolded Linux directory stores
  one spelling the walk cannot ask for.
- **Win32 propagates inheritable ACEs by name** inside `SetSecurityInfo`.

See also [[design/authority-tables|authority-tables]] for every platform's
spelling corner case, and `docs/SPEC.md` §12.2–12.4.
