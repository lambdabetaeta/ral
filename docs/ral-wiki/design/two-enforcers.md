# Two enforcers: why the guard is in-process

A capability decision is made once, in process, then enforced in up to two
places. **The two enforcers are not redundant guards over one piece of ground —
each is authoritative exactly where the other is blind.** The *in-process guard*
is complete for what ral itself dispatches; the *OS sandbox* is necessary for
what escapes that dispatch. Leaving everything to the sandbox would be strictly
weaker, not simpler.

The division falls out of one question: does ral perform the operation, or does a
spawned process perform it on its own? [[internals/capability-enforcement|capability-enforcement]]
narrates *how* each runs; this page argues *why* both exist.

**Why the sandbox cannot be the only enforcer.**

- *It never sees ral's own operations.* The bundled coreutils and the structured
  primitives execute in process — they are not subprocesses, and an OS sandbox
  confines only a child. For everything ral does itself, the in-process guard is
  the one place a check can live ([[internals/builtins-registry|builtins]]).
- *It is coarser than the grant model.* Exec is three-valued (Allow /
  Only / Deny), and the guard matches the runtime argv: `exec: [git:
  [status]]` admits `git status` and denies `git push`. The sandbox renders only
  a path rule list — it filters *which binary*, never *which subcommand*, nor
  which bundled tool a re-exec of ral runs.
  Delegating exec to the OS silently coarsens the grant.
- *It is uneven across platforms.* bwrap has no path-aware exec filter, Windows'
  container is coarse, and `net` is unscopable anywhere. Were the sandbox the
  sole enforcer, `grant` would mean different things on different kernels; the
  in-process guard is the semantic floor that makes `grant` defined by ral, with
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
guard that judges a canonicalised string and lets the caller re-walk the
original string with `open(2)` has two walks, and whatever differs between
them — a dangling link the canonicaliser could not follow, a directory swapped
for a symlink by a concurrent child — is an object the guard never saw. So the
guard's fs door is one walk (`Shell::locate`, `path/walk.rs`): from the root
through directory handles, no symlink ever followed by the kernel, each link
spliced into the name and re-walked, the verdict taken on the object landed
on, and the open, stat, staging and rename done relative to that directory
handle. Authority is preserved through execution because the handle *is* the
authority ([[decisions/260906_object-not-name|object-not-name]]). The exec
door judges the object too: a head's program is the file that will execute,
judged by its real path through the one function `ExecRules::verdict`, and
the launcher runs that real path, under the user's spelling as `argv[0]`
([[decisions/261004_exec-rules|exec-rules]]).

**Why the guard cannot be the only enforcer.**

- *It is blind to what a child does on its own.* Once ral spawns `sh`, the guard's
  work is done; `sh` may read, write, and re-exec freely. Confining that requires
  the OS.
- *Hence the asymmetry by dimension.* A spawned child's reads and writes are held
  by the sandbox; `net` has no in-process guard at all, because ral dispatches no
  network operation for the guard to see ([[design/grant|grant]]).
- *Linux exec is a Landlock domain, the denies subtracted.* bwrap cannot
  path-filter a child's re-execs, so the payload enters a Landlock layer of its
  own inside the envelope, built by the parent. Landlock is allow-list only, so
  a deny *inside* an allowed directory is rendered as that directory's entries
  the table still admits, over the tree as it stands at launch. Two gaps stay
  with the in-process guard, which sees neither once a child re-execs: a
  bare-name veto under a trusted write, which a child can sidestep by authoring
  a renamed copy and which is therefore not carried into the kernel; and the
  kernel filters the path passed to `execve`, not the code a process runs, so
  an admitted loader or interpreter handed an unadmitted file as an argument
  runs it ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]). What the
  kernel admits is one law on every platform — the guard's own rules, ordered
  so last-match-wins is its most-specific-wins precedence
  ([[related/access-control-algebra|access-control-algebra]]), plus their carriers, the platform's
  loader and ral ([[decisions/261004_exec-carriers|exec-carriers]]). Those
  rules carry only the denies a layer wrote: the meet of two tables joins
  their denies and keeps an allow only where both admit its own subject, never
  writing a default as a deny, which Seatbelt's caseless match would turn on
  every spelling of the name ([[design/authority-tables|authority-tables]]).

**What a grant's guarantee means on Linux, row by row.** Rows above the rule
are *promises*: a grant names them, and a host that cannot hold one refuses
the launch. Rows below are *invariants* of being under an envelope at all —
applied where the host can build them and stated (in SPEC and here) where it
cannot, never a reason to refuse: nothing in a grant names them, and the in-process half of process
reach is already total, a ral body reaching processes only through what ral
serves ([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).

| | held by | when the host lacks it |
|---|---|---|
| `fs` read/write prefixes, `deny` masks; what the exec table admits is readable | bwrap mounts, each source a handle ral opened without following a symlink, each mask at every name a bind shows the denied path ([[decisions/261006_the-envelope-mounts-by-handle]]) | refuse: `confinement_unavailable`, a bwrap that builds no envelope or lacks `--ro-bind-fd` (before 0.8.0) included |
| `net: false` | `--unshare-net` | refuse: `projection_enforceable` |
| `exec` — which path may be `execve`d (not which code runs), a deny inside an allowed directory included, over the tree at launch: a program added afterwards to a directory holding a block is denied until the next launch | Landlock `Execute` ruleset, the parent subtracting each block from the directory that holds it; a read-only bind over each admitted directory a write prefix covers without naming, at every name a writable bind shows it (a prefix written through a symlink is two binds of one handle), so a veto carried into it cannot be authored around ([[decisions/261006_a-veto-freezes-what-a-write-covers]]) | refuse: `confinement_unavailable`, an exec opinion alone asking for the envelope |
| `exec` — which subcommand, and a bare-name veto under a trusted write | in-process guard | the guard stands alone |
| die with parent, new session, no core, the seccomp deny-set (kills kernel attack surface; refuses mounting, user namespaces and `TIOCSTI` with an errno) | bwrap + `pre_exec` | applied where possible |
| private ipc / uts | `--unshare-*` | never refused |
| `/sys/fs/cgroup` is the payload's own tree | cgroup namespace + re-rooted bind | the tree is the host's |
| no signalling the host; host process table hidden | pid namespace + fresh `/proc` | the table is the container's own |
| private ptys | `--dev` | `/dev` by hand over the host's `/dev/pts` |

The first row has one exception: a `deny` naming a path *absent* on the host
under a *writable* prefix is held by the in-process guard alone on Linux and
Windows, no mount or ACE being able to attach to a name that does not exist
without first creating it there; a child can make the name in that launch, and
the next launch finds it and the kernel holds it. macOS holds it from the
start, Seatbelt's rules being negative over names.

The two agree on what a deny means: every spelling some filesystem takes for
its name. The guard's deny rules speak under `lex::collision_key`, its allows
under the name as stored; Seatbelt matches the same class; a bwrap mask and a Windows ACE hang on whatever object the
volume's own lookup finds, which on an existing path is the same thing. On a
case-sensitive volume the guard therefore over-denies relative to the Linux
and Windows kernels — a distinct `secrets` beside a denied `Secrets` is
refused in process and not to a child — an over-approximation, stated in the
refusal ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]];
every platform's case in [[design/authority-tables|authority-tables]]).

**macOS has no stacking.** Landlock layers inside the envelope; Seatbelt
profiles do not stack. A process already inside one gets `EPERM` entering a
second, so a confined runner's per-command child cannot host a launch under a
restricting grant: ral cannot know the entry profile holds what the grant
promises, and a host that cannot hold a promise refuses. The launch is refused
with attribution — this lineage is already profiled — never run under the wider
profile it already has.

**macOS's process row is a budget.** There is no pid namespace to hide the
host's table; a confined child instead launches with `RLIMIT_NPROC` at the
user's process count at launch plus 512, soft and hard (`sandbox::fork_brake`;
reported as a warning when the count cannot be taken, never a refusal). Darwin
counts processes per real UID but compares each `fork`/`posix_spawn` with the
*forking* process's own limit, so only the confined subtree's forks are refused
and every other process of the user keeps its own. The threshold is absolute:
if the user's session grows past it, the confined command's forks fail first,
never the desktop's. It refuses forks; it does not kill.

**Neither enforcer is the body's to rewrite.** Every launcher pinned at boot —
on Linux both bwrap and ral's own trampoline, and ral itself wherever else it
re-execs — is closed to a confined child by its own read-only bind, and to
ral's own writes by the guard's `Guarded`
verdict: judged by inode, so a hard link names it too, and *before* any grant
is folded, since a stack with no `fs` opinion is `Unrestricted` and exactly the
case to catch. It is the discard device's twin — a name no grant needs to
mention, always yes; an inode no grant can name, always no. A deny entry could
not carry it, never being reached under an open stack; a sealed-memfd copy
would lose a setuid bwrap its bit and Ubuntu's AppArmor userns profile, which
attaches to the exec'd file's path; a content digest detects the change, not
the poison, and the restart it demands pins the poisoned bytes. What stays open
— another same-uid process, a later session — a root-owned bwrap lifts
([[internals/capability-enforcement|capability-enforcement]]).

The discipline this draws: **the in-process guard is authority over dispatch, not
confinement of children.** Treating it as the latter is the mistake; pairing it
with the sandbox is the design.

This is the boundary [[design/exarch-architecture|exarch]] reuses — a run
evaluates under a grant frame whose two enforcers are exactly these.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (the guard sits where an effect is performed),
[[design/grant|grant]], [[design/scoping|scoping]],
[[design/capability-carriers|capability-carriers]] (this split as a type
distinction — the sharp judge's view vs the blunt sentry's checklist); realised in
[[internals/capability-enforcement|capability-enforcement]], and on macOS in
[[internals/seatbelt-profile|seatbelt-profile]] — an object policy rendered in
a name language, and what that costs. `docs/SPEC.md` §12.
