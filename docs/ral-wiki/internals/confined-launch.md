---
verified_at_commit: 646bff0a
verified_at_date: 2026-10-13
anchors: [sandboxed_command, build_launch, serve_warrant, Warrant, Slot, Handoff, Landlocked, Envelope, Refusal, spawn_error, projection_enforceable, SessionSandbox, ensure_frozen_grant, ensure_unreachable, fs_capability_name]
---

# Launching a confined command

**ral confines one external command at a time, at the moment it spawns it, and
nothing in that path can run a program unconfined.** When `build_launch`
(`runtime/command/process.rs`) spawns an admitted external or bundled child
under a restrictive projection, it calls `sandboxed_command`
(`sandbox/launch.rs`), which confines that one child. Why it is per command is
[[internals/capability-enforcement|capability-enforcement]].

## One confinement point per platform

- **Linux** wraps the child in bwrap ([[internals/linux-envelope|linux-envelope]]).
  The trampoline `execve`s the host file by the real path the guard judged, as
  macOS does; a `#!` script therefore sees that path as `$0`.
- **macOS** re-execs a small launcher, `ral --warrant`. The warrant carries the
  Seatbelt profile, compiled in the parent, and the program to become: a host
  file, by the real path the guard judged and with the user's spelling as
  `argv[0]`, or a bundled tool. `serve_warrant` enters Seatbelt from the profile
  alone, then `execve`s exactly that file.
- **Windows** attaches the projection's AppContainer LowBox
  `SECURITY_CAPABILITIES` to the child's own `CreateProcessW`
  (`windows::session::confine`). The parent's spawn is the confinement point;
  there is no re-exec.

## The child is handed its confinement on a descriptor

**Both Unix backends re-exec ral as `ral --warrant <fd>`, and that argv
names only a descriptor.** The confinement and the program arrive in a
`Warrant` the parent compiled, on that descriptor.

- **No other process can write the channel.**
  - On Linux it is a sealed memfd. A same-uid process may reopen any descriptor
    through `/proc`, and a sealed copy is as read-only as ours. The parent reads
    it back and checks it once sealed.
  - On macOS, which has no `/proc`, it is the read end of a socketpair whose
    writer closes before the child exists.
- **The child refuses anything unexpected:** an unsealed memfd, a non-socket, or
  a warrant not in canonical form. Decoding re-encodes and compares.
- **On Linux the real argv is `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w>`.**
  `bwrap_argv` is pure in descriptor numbers and returns only bwrap's
  options, which bwrap reads from `--args`. The trampoline is ral's own pin.
- **Every descriptor a launch hands down crosses at the number it was lent
  at** (`Slot`, from 98 up): bwrap's `--args`, the warrant, `--info-fd` and
  the payload's copy of its writer, the Landlock ruleset, ral's pin, the
  seccomp programs and the mount handles. A `Handoff` lifts each to a free
  number in the parent, and the child's hook only clears close-on-exec:
  nothing is `dup2`'d, so no descriptor the child still needs (`std`'s
  exec-error pipe, `detach`'s handshake) can be displaced. Each number is
  named as data: to bwrap in its options, the ruleset's in the warrant,
  the warrant's in argv. Once confined, the child closes everything from 98
  up, so the program inherits none, and a `Kept` launch's read of
  `--info-fd` ends: the payload's group exists.
- **`serve_warrant` runs before `sandbox::boot`.** The trampoline pins and
  opens nothing before it is confined.
- **Exit codes are honest.** A failure before the program starts exits 126, a
  missing program 127. An `execve` refusal takes its code from the same
  `SpawnFailure` the in-process spawn uses.

## Failing closed is the whole stance

**`Warrant::confine` is the only road to the program, and it runs only after the
confinement is entered and the handoff closed.** The warrant's `Confinement` is
the one thing the child enters: `Seatbelt(profile)` on macOS; on Linux
`unconfined`, `landlock@<fd>` or `landlock+refer@<fd>`, a `Landlocked` promise naming the ruleset's descriptor, or none.

- **On Linux the parent decides everything.** It probes Landlock once, and
  `landlock::build`:
  - refuses every launch if the probe failed;
  - refuses a restricting exec grant where Landlock is absent;
  - creates the ruleset with exactly the handled set the kernel accepts;
  - admits each rule by a handle opened without following symlinks. A file
    admit that has become a directory is dropped, not widened to a hierarchy,
    and one now reached through a symlink refuses the launch.
- **The child probes nothing.** It takes the ruleset from its slot, adds
  `Refer` on its own root when promised (that root exists only inside), and
  enters it. A promised ruleset that never arrived, or a descriptor that is not
  one, refuses the launch.
- **A sandboxed launch needs ral's own pin** (`reexec::own`, fixed at
  `sandbox::boot`, so a swap on disk cannot subvert it). Without the pin, the
  launch is refused.
  - On Linux bwrap execs that pin by descriptor. The trampoline is the boot
    inode whatever holds its name now, and nothing re-stats the name. A build
    swapped in mid-session runs under the parent that pinned the old one,
    consistently. A filesystem that cannot exec an unlinked file (virtiofs)
    fails the launch instead. The trampoline runs before the payload enters its
    policy: trusted construction, not an authored exec. No policy admits ral's
    image on its own account; ral's binary is an ordinary file the table admits
    or not ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
  - macOS execs the pin by name, so it re-verifies it first (`Pinned::verify`).

## Windows keys authority by path, and the token selects

**Each `(canonical path, kind)` grant derives a fixed capability SID from a hash
of the canonical path** (`dacl::fs_capability_name`). Its ACE is stamped once,
ever, and never reverted (`dacl::ensure_frozen_grant`). `session::confine` mints
into the child's token exactly the capability SIDs of this launch's grants.

- **The kernel enforces the projection the guard judges.** A narrowed grant, or
  a subagent's narrowed permissions, hold at the OS layer, because the narrower
  token lacks the wider paths' capabilities.
- **Stamps that stay are safe.** A capability SID is evaluated only in the
  AppContainer pass of the access check, whose result intersects the normal user
  pass. An ACE no live token names is inert, and can never widen a process's
  reach past its owning user's. So a worker keeps the authority it was
  born with, by construction.
- **A frozen allowance is stamped through a held handle, never by name.** Its
  real path was frozen at decode, and a stamp by name would follow a junction
  planted on the path or on any ancestor and leave a window between check and
  stamp. So the invariant is: resolution meets no redirecting reparse point
  anywhere on the path, and the grant lands on the very object checked. The
  walk holds the position root first, each component opened relative to its
  parent's handle without following, without delete sharing; only a name
  surrogate (symlink, junction, mount point) refuses the launch, naming both,
  and a cloud-files placeholder passes, which `OBJ_DONT_REPARSE` would have
  refused. The leaf's stamping handle is derived from the walk's by
  `ReOpenFile`, and a share is refused by asking that handle for its volume
  (`dacl/frozen.rs`). A position that is not Unicode is refused there too, as
  `DaclError::NotUnicode`: the capability name is a hash of the position's text,
  and a grant that cannot be expressed faithfully is refused, not approximated.
- **The check comes before every shortcut, on every launch.** Three things
  spare a stamp: a path not on disk, the session's memo of the grants it has
  ensured, and the ambient-access probe, which spares the capability ACE alone.
  A path granted at an earlier launch and since replaced by a junction would
  pass all three and skip a check placed in the stamp alone, so every frozen
  allowance is checked first, warm cache or not. A read-write grant on a
  directory also sets a mandatory integrity label, ambient access or not, since
  the mandatory check runs before any ACE is read; it goes through the held
  handle too, so both operations reach the object checked.
- **A token names its own launch's grants and nothing else.** Its capabilities
  are derived afresh at each launch, the program image and the carving of the
  tree as it stands included, so nothing one launch was granted reaches another
  launch under the same projection. The projection key, its whole fs lists,
  picks only the profile: the package SID and its named-object namespace. Every
  run place is stamped as a read prefix is, frozen. The guest-jail launch branch
  still bypasses per-command projection and installs no kernel exec policy.
- **An exclusion is never stamped.** The AppContainer pass ignores a deny ACE
  naming a capability, so nothing can take back what a grant's ACE admits.
  `session::confine` carves an exclusion out of the allowances around it,
  granting their existing entries less the excluded ones and each directory on
  the way only its own listing by an ACE nothing inherits (`sandbox::carve`),
  and refuses one on or around an allowance, the program image among them,
  before anything is stamped; for every exclusion it then asks `AuthzAccessCheck`, with the child's own package and capability SIDs,
  whether the token could open anything beneath it, a file or a directory held
  by the walk, each classified by what the hold acquired rather than by its
  listing, and read through handles derived from it, and refuses the launch,
  naming the object, if so (`dacl::ensure_unreachable`;
  [[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]],
  amendment of 2026-10-20).
- **A bundled tool's child is `ral.exe` itself**, loaded by the AppContainer,
  whose image keeps its read and execute grant: the spawn's, by the parent, not an
  exec judged in a child. An fs exclusion over `ral.exe`'s folder holds the
  image, so `confine` refuses the launch, as the guard already refuses every
  bundled tool in that case; Windows needs no exception.
- **Residual:** an ACE lives on the NTFS object, but a grant names a path, and
  Windows does not re-inherit on a same-volume rename. A file moved into a
  granted tree stays dark; one moved out of a writable tree keeps that tree's
  capability. Path rules and object stamps agree only while the tree stays
  still ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).

## When confinement is unavailable

**A launch that cannot be confined is refused, and the refusal says the sandbox
is why** (`sandbox::Refusal::Unavailable`).

- **Some causes show only at spawn.** On Linux the envelope is bwrap, a host
  package, and a host without it fails at the spawn with `ENOENT`.
- **The `ENOENT` is the envelope's, never the target's.** `vet` has already
  found the target on `PATH`, and an envelope execs its target itself,
  reporting a missing one as an exit status. So under an active projection a
  spawn-time `NotFound` can only be the envelope's, and `spawn_error` says so.
  Reading it as the target's would accuse the one program known to exist, and
  make a host without bubblewrap look like a grant that denies everything.
- **The launch records which envelope it execs** (an `Envelope`, read back by
  `Launch::confinement`). Only a backend whose envelope is a separate binary
  sets one; Windows has none. Blame is read off
  what the launcher did, not re-derived from the shell's state.
- **An offline request on a backend that cannot enforce it** fails closed at
  the spawn (`projection_enforceable`).

## Related rules

- **A bundled coreutil is never inlined under a restrictive grant.** Its
  filesystem access has no in-process guard, so it is spawned as a re-exec child
  of ral (`--ral-bundled-tool <tool>`, or a warrant naming the tool) and gets the
  same per-command sandbox as any external
  ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]];
  [[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- **A ral-written pipeline stage is not a launch.** It runs on its own thread of
  the parent and shares the parent's memory; it is never a re-exec'd child
  ([[internals/pipeline-execution|pipeline execution]];
  [[decisions/260902_stages-are-threads|stages-are-threads]]).
- **Any synchronous child wait a host still performs must have an out-of-band
  cancellation path.** A parent blocked reading a framed response cannot see its
  own foreground `Scope` by polling, since the poll never runs while the
  read is parked. Deadline and Esc can break a wedged wait only if the parent
  has a side channel that signals the confined child's subtree from outside.
  Giving the child more signal authority is no substitute: it lets the child
  signal its own descendants, but does nothing for a parent stuck on the IPC
  edge.
