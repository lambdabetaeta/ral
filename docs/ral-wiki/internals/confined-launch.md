---
generated_at_commit: f1b0a4a4
verified_at_commit: f1b0a4a4
verified_at_date: 2026-11-07
anchors: [sandboxed_command, build_launch, serve_warrant, Warrant, Slot, Handoff, Landlocked, Envelope, unavailable, Unstarted, unforked, vanished, Testimony, Witness, Confined, supervise, SessionSandbox, ensure_frozen_grant, ensure_unreachable, fs_capability_name, lock_inheritance]
---

# Launching a confined command

**A host projection confines one external command at its spawn boundary.**
Linux guest engines use the [[internals/capability-enforcement#Guest jail|guest jail]]
instead: the utility and its descendants keep the jail and VM perimeter, but
receive no per-grant fs/exec projection. Seccomp and, when requested, offline
network namespaces still apply there.

When `prepare` builds the launch of an admitted external or bundled child
(`build_launch`, `shell/exec/command.rs`) on a host under a restrictive
projection, it calls `sandboxed_command`
(`authority/sandbox/launch.rs`), which describes that one child's
confinement. Why it is per command is
[[internals/capability-enforcement|capability-enforcement]].

**Building describes; the spawn confines** ([[design/exec|exec]]). Everything
that may be stopped runs in `Launch::spawn` (`spawn_detached`), under the scope
`start` hands it; the build waits on nothing a stop must end.

- **Unix builds** render the projection, open the binds, create the Landlock
  ruleset, seal the warrant, open the report socket and register the `pre_exec`
  hooks: descriptors of this process, and hooks in their fixed order. A guest
  jail instead creates its cgroup and registers its hook in that order. The
  linker glob is a bounded walk of a handful of library directories and polls
  nothing; the bwrap host probe runs its bwrap children once per process and
  caches the answer. The spawn forks, then waits for the first word.
- **Windows builds** a `Confined` that only describes the token: the
  projection and the image. Every spawn runs `session::confine` (profile,
  carving, stamps, unreachability proof) and hands the token it returns
  straight to `CreateProcessW`, so no launch holds a token apart from its
  confinement and a retry confines again. A stop is seen before each stamp
  and between directories of each proof; a stamp's own Win32 propagation
  runs to its end.

On Unix, `build_launch` registers the session's `ChildLimits` hook immediately
after `apply_env`, before `limit_resources`. `pre_exec` hooks run in registration
order: session limits set only the soft ceiling, preserving the hard ceiling;
a grant sets soft and hard together afterward and therefore wins. ral's own
resource limits never change.
The grant's limit is part of the launch, applied inside the platform's spawn
([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

## One confinement point per platform

- **Linux hosts** wrap the child in bwrap ([[internals/linux-envelope|linux-envelope]]).
  The trampoline runs the host file by the real path the guard judged, as macOS
  does, but as its supervised child and not in its own place; a `#!` script
  therefore sees that path as `$0`.
- **macOS** re-execs a small launcher, `ral --warrant`. The warrant carries the
  Seatbelt profile, compiled in the parent, and the program to become: a host
  file, by the real path the guard judged and with the user's spelling as
  `argv[0]`, or a bundled tool. `serve_warrant` enters Seatbelt from the profile
  alone, then `execve`s exactly that file.
- **Windows** attaches the projection's AppContainer LowBox
  `SECURITY_CAPABILITIES` to the child's own `CreateProcessW`
  (`authority::sandbox::windows::session::confine`, run by `Launch::spawn`). The parent's spawn is the confinement point;
  there is no re-exec. One process-wide inheritance mutex covers ral's raw
  launches and helper-backed standard spawns through creation and restoration
  of temporary inherit flags, not Job Object placement/resume or child waits.
  The explicit handle list restricts the intended raw child only; it cannot
  constrain another inheriting child. Unrelated dependency spawns do not take
  ral's mutex and remain outside this serialization.

## The child is handed its confinement on a descriptor

**Both Unix host backends re-exec ral as `ral --warrant <w> <r>`, and that argv
names only two descriptors.** The confinement and the program arrive in a
`Warrant` the parent compiled, on the first; the second is the launch's report
socket, on which the trampoline says what became of the program
([[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]).

- **No other process can write the channel.**
  - On Linux it is a sealed memfd. A same-uid process may reopen any descriptor
    through `/proc`, and a sealed copy is as read-only as ours. The parent reads
    it back and checks it once sealed.
  - On macOS, which has no `/proc`, it is the read end of a socketpair whose
    writer closes before the child exists.
- **The child refuses anything unexpected:** an unsealed memfd, a non-socket, or
  a warrant not in canonical form. Decoding re-encodes and compares.
- **On Linux the real argv is `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w> <r>`.**
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
  the warrant's in argv. The report socket's writer crosses at its own slot, which the parent holds open
  until the fork ([[internals/an-external-end-to-end|an-external-end-to-end]]).
  Once confined, the child closes every descriptor above stderr but the
  report's and marks that one close-on-exec, so the program inherits none, and a
  `Kept` launch's read of `--info-fd` ends: the payload's group exists.
- **`serve_warrant` runs before `authority::sandbox::boot`.** The trampoline pins and
  opens nothing before it is confined.
- **The trampoline testifies; it does not exit with a verdict.** The trampoline
  prints nothing once it holds a report socket, and the parent words what it is
  told; only a call that names no report socket, made by hand, is refused on
  stderr.

## The trampoline testifies, and on Linux supervises

**The parent believes only what the report says, and hears the first word
before it believes the program started.** The life of a launch from the parent's
side is [[internals/an-external-end-to-end|an-external-end-to-end]]; this is the
trampoline's. Its code is `core/src/authority/sandbox/warrant/trampoline.rs`;
`warrant.rs` is the codec only (`warrant/handoff.rs` holds the descriptors
handed down), and the warrant carries the `Image` verbatim.

- **The records** and what each may say are
  [[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]'s. `refused` is the
  warrant's failure to be received or its confinement to be entered; `unstarted`
  is the program's own spawn or `execve` failing once the confinement was
  entered; `started` and `ended` are Linux's.
- **On macOS the trampoline becomes the program**: a host file is `execve`d in
  place, and the report's close-on-exec writer closing is the start; a bundled
  tool runs here, after the report is dropped. The program keeps the child's
  pid and its exact wait status.
- **On Linux the trampoline is a supervisor** and never `execve`s
  ([[invariants/a-supervisor-never-execs|a-supervisor-never-execs]]; the choice
  is [[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]]).
  It spawns the host file as its child, or forks the bundled tool, says
  `started`, waits, and says `ended` with the exact status.
- **The ending is amended at the reaper**, from the testimony the child's
  handle carries
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
- **A stop ends the wait for the first word**, and the child is discarded
  ([[invariants/a-first-word-is-waited-for-under-the-scope|a-first-word-is-waited-for-under-the-scope]]).

## Failing closed is the whole stance

**For a Unix host projection, `Warrant::confine` is the only road to the program;
it runs only after confinement is entered and the handoff closed.** The warrant's `Confinement` is
the one thing the child enters, one type per platform: the profile on macOS; on Linux
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
  `authority::sandbox::boot`, so a swap on disk cannot subvert it). Without the pin, the
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

The two halves of Windows' [dual-principal access check](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)
must both grant access. An `Everyone` ACE supplies ordinary user/group rights,
not AppContainer authority, so it never excuses the capability stamp. Only
ambient package principals (`ALL APPLICATION PACKAGES` and `ALL RESTRICTED
APPLICATION PACKAGES`) can satisfy that shortcut.

- **The kernel enforces the projection the guard judges.** A narrowed grant, or
  a subagent's narrowed permissions, hold at the OS layer, because the narrower
  token lacks the wider paths' capabilities.
- **An image grant belongs to one launch and one file.** It admits reading and
  executing that image, not enumerating its parent directory. The isolation
  regression uses a native child to read the exact file, then launches under
  the same projection without that file's capability and requires access denied.
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
  refused. The leaf's stamping handle is derived from the walk's by an
  empty-name `NtCreateFile` relative to that held handle, including for a
  directory; a share is refused by asking the handle for its volume
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
  run place is stamped as a read prefix is, frozen.
- **An exclusion is never stamped.** The AppContainer pass ignores a deny ACE
  naming a capability, so nothing can take back what a grant's ACE admits.
  `session::confine` carves an exclusion out of the allowances around it,
  granting their existing entries less the excluded ones and each directory on
  the way only its own listing by an ACE nothing inherits (`authority::sandbox::carve`),
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

**A host launch whose required projection cannot be installed is refused, and
the refusal says the sandbox is why** (`sandbox::unavailable`).

- **A host without bwrap is refused at build.** `linux::envelope()` reports the
  pin boot did not find, and `linux_sandboxed_command` refuses with it. Other
  causes show only at spawn.
- **A launcher that fails to exec is the envelope's failure, never the
  target's.** `prepare`'s probe has already found the target on `PATH`, so a
  spawn error of a confined launch's launcher, `ENOENT` included, is `Refused`
  (`unforked`, on an `Exec` failure), worded as the sandbox being unavailable
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).
- **An envelope that ends before its trampoline speaks is a refusal too**,
  worded with whatever it left on the command's stderr (`vanished`):
  on Linux, EOF on the report before any record.
- **The launch records which envelope it execs** (an `Envelope`, held in its
  `Confined` beside the report, and read back by
  `Confined::launcher`). Only a backend whose envelope is a separate binary sets
  one; Windows has none. Blame is read off what the launcher did, not
  re-derived from the shell's state.

## Related rules

- **Bundled coreutils and rg run in re-exec children, not the evaluator.**
  Their filesystem access has no in-process guard. The child of ral
  (`--ral-bundled-tool <tool>`, or a warrant naming the tool) takes the same
  host-projection or guest-jail branch as an external file
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
