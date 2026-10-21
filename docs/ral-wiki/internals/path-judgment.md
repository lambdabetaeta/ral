---
verified_at_commit: f56fff48
verified_at_date: 2026-10-21
anchors: [locate, walk, resolve_lenient, Missing, Located, LexicalPath::folded, check_fs_exact, collision_key, is_discard, check_device_name, admits_fs, FsRefusal, pinned_binary]
---

# Path judgment: what ral checks when it touches a file

**ral judges the object an access reaches, not the name it was spelled with**
([[decisions/260906_object-not-name|object-not-name]]). This page is the
in-process half of [[internals/capability-enforcement|capability-enforcement]]
for the filesystem. The code is `core/src/path/` and `core/src/guard/`.

## A read by name: four stages

**A path is matched by a fixed rule:**

1. expand sigils and `~`;
2. anchor against the logical cwd, `.`, `..` and a trailing separator kept;
3. canonicalise leniently: the walk under `Missing::Tolerate`;
4. match by prefix (`path_within`).

Because ral canonicalises *before* it matches, a symlink or a `..` cannot lead
out of a directory a grant scopes. This rule serves a read *by name*: a
predicate, a listing, a module load.

- **Anchoring is not folding.** A logical consumer (`cd`, `within [dir: …]`,
  `absolute-path`, a display base) folds `.` and `..` explicitly
  (`LexicalPath::folded`). What reaches the disk is handed unfolded
  ([[decisions/261021_access-is-physical|access-is-physical]]).
- **Stage 3 is the walk, tolerant of a missing tail.** It resolves each `..`
  physically, after the links before it, and a dangling link at its target;
  at the first name it cannot enter it appends the rest lexically. So a policy
  and an open resolve a name by the same code, and a deny on a dangling link
  names what a write through it creates.

## An open: a fifth stage, the walk

**Every open goes through `Shell::locate`, which walks the name itself and
judges the object it lands on** (`path/walk.rs`). `check_fs_write` does not
exist: every write is a `locate`.

- **The walk starts at the root and holds *search* handles.** A search handle is
  the right to resolve names through a directory, not to read it. So the walk
  asks of each ancestor exactly what the kernel's own resolver asks, and a
  sandbox rule that grants ancestors metadata only is enough.
- **The kernel never follows a symlink.** The walk splices each link it meets
  into the rest of the name itself. A `..` is the kernel's and physical: it
  climbs out of the link's target, not out of the link's place. Every
  restart, the given name's and each splice's, passes the one `rooted`
  check, so a target with no root to walk from (relative to a drive's
  current directory, or a head `std::path` cannot parse) is refused as the
  name itself would be.
- **The walk judges the object, then touches that object.** `check_fs_exact`
  judges `Located::real`, which is canonical by construction. The `Located`
  then does the open, stat, staging, rename or unlink relative to its directory
  handle, with `FollowSymlinks::No`. So what was judged is what is touched: a
  dangling link is judged at its target, and a component swapped after the
  judgment is never followed.
- **A directory is an object too.** The root, a name ending in a separator and
  a trailing `..` locate the directory itself, through its own handle. A
  separator demands a directory, a link's target carrying the demand, so
  `file/` is the kernel's `ENOTDIR`; a write to a directory is its `EISDIR`.

## An allowance is frozen; an exclusion re-resolves

**An allowance keeps the real path it was frozen to at decode and is never
re-frozen; an exclusion is re-frozen against the live disk on every question.**
So a symlink put in an allowance's place after the grant redirects nothing,
while a new spelling of an excluded object is excluded the moment it exists
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
Freezing refuses a real path that is not Unicode. A read also reads what a
grant, and the grants enclosing it, let run, and every write prefix.

## A deny holds under every spelling

**One file can have several names. A deny holds under all of them; an allow
holds as frozen**
([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]).

- **On macOS** the walk spells each component as the directory stores it. A
  case-variant name therefore meets a deny frozen under its stored spelling.
- **On Windows** the kernel renames a `~`-bearing name through an
  attribute-only handle. An 8.3 alias therefore meets a deny frozen under its
  long name.
- **For a name that does not exist**, stored spellings cannot help. Here the
  deny relation does the work: a `Region`'s deny rule holds any path whose
  components agree with it under `identity::collision_key`. So creating `.ENV`
  meets an absent `` deny: [`cwd .env] ``, and the refusal says another spelling
  caught it.

## Exceptions decided before any region

**One target needs no authority, and one write no grant can admit.** Both are
decided before the stack is consulted.

- **The discard device is always allowed.** `/dev/null`, or `\\.\NUL` on
  Windows, is named by `LexicalPath::is_discard` on either host. Nothing reaches
  the disk through it. The same predicate decides whether the act counts as a
  fact at all ([[design/audit|audit]]).
- **A DOS device name is refused, not excused.** A path that ends in a reserved
  device name (`C:\x\NUL`, `nul.txt`, `CON`, `COM1.log`) is no file ral will
  open or judge. The same door turns it away with an error before any region is
  consulted (`check_device_name`).
- **`GrantStack::admits_fs` does not share the exemption.** It decides
  *membership* of a region, and a device excused from an access is not thereby
  a member of anything.
- **A write onto a pinned binary is refused.** the fs judgment answers `FsRefusal::Guarded`
  for a write onto a binary the sandbox pinned at boot: bwrap on Linux, and
  ral's own executable. It answers *before* the stack is folded, because a stack
  with no `fs` opinion is `Unrestricted`, which is exactly the case to catch.
  `sandbox::pinned_binary` judges by inode, so a hard link or a rename since
  boot names the pin too. Replacing the file by rename stays allowed: the pinned
  copy survives it ([[design/two-enforcers|two-enforcers]];
  [[internals/linux-envelope|linux-envelope]] for the kernel side).

See also [[design/authority-tables|authority-tables]] for every platform's
spelling corner case, and `docs/SPEC.md` §12.3–12.4.
