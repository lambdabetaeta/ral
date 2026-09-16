# Two enforcers: why the gate is in-process

A capability decision is made once, in process, then enforced in up to two
places. **The two enforcers are not redundant guards over one piece of ground —
each is authoritative exactly where the other is blind.** The *in-process gate*
is complete for what ral itself dispatches; the *OS sandbox* is necessary for
what escapes that dispatch. Leaving everything to the sandbox would be strictly
weaker, not simpler.

The division falls out of one question: does ral perform the operation, or does a
spawned process perform it on its own? [[internals/capability-enforcement|capability-enforcement]]
narrates *how* each runs; this page argues *why* both exist.

**Why the sandbox cannot be the only enforcer.**

- *It never sees ral's own operations.* The bundled coreutils and the structured
  primitives execute in process — they are not subprocesses, and an OS sandbox
  confines only a child. For everything ral does itself, the in-process gate is
  the one place a check can live ([[internals/builtins-registry|builtins]]).
- *It is coarser than the grant model.* Exec is three-valued (Allow /
  Subcommands / Deny), and the gate matches the runtime argv: `exec: [git:
  [status]]` admits `git status` and denies `git push`. The sandbox renders only
  a path allow/deny list — it gates *which binary*, never *which subcommand*.
  Delegating exec to the OS silently coarsens the grant.
- *It is uneven across platforms.* bwrap has no path-aware exec filter, Windows'
  container is coarse, and `net` is unscopable anywhere. Were the sandbox the
  sole enforcer, `grant` would mean different things on different kernels; the
  in-process gate is the semantic floor that makes `grant` defined by ral, with
  the OS layer tightening further where it can.
- *Its denials are opaque, and it is not free.* An in-process rejection is a
  structured escape carrying an [[design/audit|audit]] fragment — ral names which
  grant denied what, and exarch's attend loop receives it as a value to reason
  about. A sandbox violation is an `EPERM` or a `SIGKILL`, after the fact and
  unattributable. And when no layer restricts fs, net, or — where a backend
  renders it into the kernel — exec, the projection is empty, so the
  per-command sandbox launch is skipped entirely: an unrestricted child spawns
  directly.

**What the in-process enforcer authorises is the object, not the name.** A
gate that judges a canonicalised string and lets the caller re-walk the
original string with `open(2)` has two walks, and whatever differs between
them — a dangling link the canonicaliser could not follow, a directory swapped
for a symlink by a concurrent child — is an object the gate never saw. So the
gate's fs door is one walk (`Shell::locate`, `path/walk.rs`): from the root
through directory handles, no symlink ever followed by the kernel, each link
spliced into the name and re-walked, the verdict taken on the object landed
on, and the open, stat, staging and rename done relative to that directory
handle. Authority is preserved through execution because the handle *is* the
authority ([[decisions/260906_object-not-name|object-not-name]]).

**Why the gate cannot be the only enforcer.**

- *It is blind to what a child does on its own.* Once ral spawns `sh`, the gate's
  work is done; `sh` may read, write, and re-exec freely. Confining that requires
  the OS.
- *Hence the asymmetry by dimension.* A spawned child's reads and writes are held
  by the sandbox; `net` has no in-process gate at all, because ral dispatches no
  network operation for the gate to see ([[design/grant|grant]]).
- *Linux exec is a Landlock domain, minus the denies.* bwrap cannot path-filter
  a child's re-execs, so the payload enters a Landlock layer of its own inside
  the envelope. Two gaps stay with the in-process gate, which sees neither once
  a child re-execs: Landlock is allow-list only, so a deny *inside* an admit is
  the gate's alone; and the kernel gates the path passed to `execve`, not the
  code a process runs, so an admitted loader or interpreter handed an
  unadmitted file as an argument runs it
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).

**What a grant's guarantee means on Linux, row by row.** Rows above the rule
are *promises*: a grant names them, and a host that cannot hold one refuses
the launch. Rows below are *invariants* of being under an envelope at all —
applied wherever the host can build them, reported where it cannot
(`HostEnvelope`, printed by `RAL_DUMP_SANDBOX_PROFILE`), never a reason to
refuse: nothing in a grant names them, and the in-process half of process
reach is already total, a ral body reaching processes only through what ral
serves ([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).

| | held by | when the host lacks it |
|---|---|---|
| `fs` read/write prefixes, `deny` masks | bwrap mounts | refuse: `confinement_unavailable` |
| `net: false` | `--unshare-net` | refuse: `projection_enforceable` |
| `exec` — which binary | Landlock `Execute` ruleset | refuse: `confinement_unavailable`, an exec opinion alone asking for the envelope |
| `exec` — which subcommand, and a deny inside an allow | in-process gate | the gate stands alone |
| die with parent, new session, no core, nproc cap, the seccomp deny-set (kills kernel attack surface; refuses mounting, user namespaces and `TIOCSTI` with an errno) | bwrap + `pre_exec` | applied where possible |
| private ipc / uts | `--unshare-*` | never refused |
| `/sys/fs/cgroup` is the payload's own tree | cgroup namespace + re-rooted bind | reported: the tree is the host's |
| no signalling the host; host process table hidden | pid namespace + fresh `/proc` | reported: the table is the container's own |
| private ptys | `--dev` | reported: `/dev` by hand over the host's `/dev/pts` |

The first row has one exception: a `deny` naming a path *absent* on the host
under a *writable* prefix is held by the in-process gate alone, no mount being
able to mask a name that does not exist without first creating it there.

**Neither enforcer is the body's to rewrite.** Every launcher pinned at boot —
on Linux both bwrap and ral's own trampoline, and ral itself wherever else it
re-execs — is closed to a confined child by its own read-only bind, and to
ral's own writes by the gate's `Guarded`
verdict: judged by inode, so a hard link names it too, and *before* any grant
is folded, since a stack with no `fs` opinion is `Unrestricted` and exactly the
case to catch. It is the discard device's twin — a name no grant needs to
mention, always yes; an inode no grant can name, always no. A deny entry could
not carry it, never being reached under an open stack; a sealed-memfd copy
would lose a setuid bwrap its bit and Ubuntu's AppArmor userns profile, which
attaches to the exec'd file's path; a content digest detects the change, not
the poison, and the restart it demands pins the poisoned bytes. What stays open
— another same-uid process, a later session — the profile dump names, with its
remedy: a root-owned bwrap
([[internals/capability-enforcement|capability-enforcement]]).

The discipline this draws: **the in-process gate is authority over dispatch, not
confinement of children.** Treating it as the latter is the mistake; pairing it
with the sandbox is the design.

This is the boundary [[design/exarch-architecture|exarch]] reuses — a run
evaluates under a grant frame whose two enforcers are exactly these.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (the gate sits where an effect is performed),
[[design/grant|grant]], [[design/scoping|scoping]],
[[design/capability-carriers|capability-carriers]] (this split as a type
distinction — the sharp judge's view vs the blunt guard's checklist); realised in
[[internals/capability-enforcement|capability-enforcement]], and on macOS in
[[internals/seatbelt-profile|seatbelt-profile]] — an object policy rendered in
a name language, and what that costs. `docs/SPEC.md` §12.
