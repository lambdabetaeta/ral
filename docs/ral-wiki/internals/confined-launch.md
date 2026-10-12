---
verified_at_commit: 2afefdf4
verified_at_date: 2026-10-12
anchors: [sandboxed_command, build_launch, serve_warrant, Warrant, Slot, Handoff, Landlocked, Envelope, Refusal, spawn_error, projection_enforceable, SessionSandbox, ensure_fs_grant, fs_capability_name]
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
  `Head::resolve` opens the image with `O_PATH` before admission and judges
  the name of that open object. The descriptor reaches the trampoline at
  `Slot::Image` and is explicitly admitted when Landlock restricts exec.
  Rulesets that only restrict other operations need no image rule. Inside the envelope,
  `visible_image` opens the real path without following symlinks and checks
  its inode against that pin. A changed or masked image refuses the launch;
  a matching image is exec'd by this new descriptor. It belongs to the
  envelope's mount, so reopening it cannot evade a read-only bind or deny.
  Unconfined launches use the original pin through `image_command`.
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

**Both Unix backends re-exec ral as `ral --warrant`, and that argv names
nothing.** The confinement and the program arrive in a `Warrant` the parent
compiled, on fd 99.

- **No other process can write the channel.**
  - On Linux it is a sealed memfd. A same-uid process may reopen any descriptor
    through `/proc`, and a sealed copy is as read-only as ours. The parent reads
    it back and checks it once sealed.
  - On macOS, which has no `/proc`, it is the read end of a socketpair whose
    writer closes before the child exists.
- **The child refuses anything unexpected:** an unsealed memfd, a non-socket, or
  a warrant not in canonical form. Decoding re-encodes and compares.
- **On Linux the real argv is `bwrap --args 98 -- /proc/self/fd/102 --warrant`.**
  `bwrap_argv` is pure in descriptors and returns only bwrap's options, which
  bwrap reads from `--args`. The trampoline is ral's own pin, lent at fd 102.
- **Every descriptor a launch hands down has a slot defined once** (`Slot`):
  bwrap's `--args`, the warrant, `--info-fd`, the Landlock ruleset, ral's pin,
  the seccomp programs, the image at fd 107, and the mount handles from fd 108,
  which come last because their
  number is open-ended. One `Handoff` places them all. Once confined, the child
  moves the image below the handoff range and closes everything from the
  first slot up. The image alone survives exec: a shebang interpreter must
  reopen it. The interpreter receives `/proc/self/fd/N` as the script's name,
  so a script's `$0` is that descriptor path; native binaries keep the user's
  spelling as `argv[0]`. An in-place rewrite of the inode is outside this pin's
  guarantee, as are the platform's interpreter lookups.
- **`serve_warrant` runs before `sandbox::boot`.** The trampoline pins and
  opens nothing before it is confined.
- **Exit codes are honest.** A failure before the program starts exits 126, a
  missing program 127. An `execve` refusal takes its code from the same
  `SpawnFailure` the in-process spawn uses.

## Failing closed is the whole stance

**`Warrant::confine` is the only road to the program, and it runs only after the
confinement is entered and the handoff closed.** The warrant's `Confinement` is
the one thing the child enters: `Seatbelt(profile)` on macOS; on Linux
`unconfined`, `landlock` or `landlock+refer`, a `Landlocked` promise or none.

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
    fails the launch instead. The trampoline's own exec admit is that pin's
    descriptor, never a name.
  - macOS execs the pin by name, so it re-verifies it first (`Pinned::verify`).

## Windows keys authority by path, and the token selects

**Each `(canonical path, kind)` grant derives a fixed capability SID from a hash
of the canonical path** (`dacl::fs_capability_name`). Its ACE is stamped once,
ever, and never reverted (`dacl::ensure_fs_grant`). `session::confine` mints
into the child's token exactly the capability SIDs its projection names, from
the session's `SessionSandbox`.

- **The kernel enforces the projection the guard judges.** A narrowed grant, or
  a subagent's narrowed permissions, hold at the OS layer, because the narrower
  token lacks the wider paths' capabilities.
- **Stamps that stay are safe.** A capability SID is evaluated only in the
  AppContainer pass of the access check, whose result intersects the normal user
  pass. An ACE no live token names is inert, and can never widen a process's
  reach past its owning user's. So a detached worker keeps the authority it was
  born with, by construction.
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
  own foreground `CancelScope` by polling, since the poll never runs while the
  read is parked. Deadline and Esc can break a wedged wait only if the parent
  has a side channel that signals the confined child's subtree from outside.
  Giving the child more signal authority is no substitute: it lets the child
  signal its own descendants, but does nothing for a parent stuck on the IPC
  edge.
