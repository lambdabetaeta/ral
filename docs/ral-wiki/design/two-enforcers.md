---
generated_at_commit: 104cb519
---

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
- *It is coarser than the grant model.* The guard judges a launch by its
  target, a bundled tool by name, and a bare-name veto under a trusted write;
  the sandbox renders only a path rule list, and never sees which bundled tool a
  re-exec of ral runs. Delegating exec to the OS silently coarsens the grant.
- *It is uneven across platforms.* bwrap has no path-aware exec filter, Windows'
  container is coarse, and `net` is unscopable anywhere. Were the sandbox the
  sole enforcer, `grant` would mean different things on different kernels; the
  in-process guard is the semantic floor that makes `grant` defined by ral, with
  the OS layer tightening further where it can.
- *Its denials are opaque, and it is not free.* An in-process rejection is a
  structured escape carrying an [[design/audit|audit]] fragment — ral names which
  grant denied what, and exarch's attend loop receives it as a value to reason
  about. A sandbox violation is an `EPERM` or a `SIGKILL`, after the fact and
  unattributable. And when no layer restricts fs, net, gpu, or — where a backend
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
judged by its real path through the one exec judgment (`GrantStack::admit`), and
the launcher runs that real path, under the user's spelling as `argv[0]`
([[decisions/261004_exec-rules|exec-rules]]).

**The two enforcers agree on names and differ on identity, and the difference is
ral's.** An exclusion is fixed, when the grant is decoded, to the object its
spelling reached; ral's own readers (`cat`, `grep`, the editor, the tree walk,
a redirect) match it by position *or* by that object, so a denied file
renamed or hard-linked under an allowed directory is still refused to them.
The kernel renderers consume the same `RealPath` and `Hold` and still judge by
position: a bwrap mask, a Seatbelt rule or a DACL carve names the place, so
that file is visible to a program ral runs. The sequel plan,
`261021_enforcers-agree`, is where the renderers come to agree with ral's own
readers ([[decisions/261021_one-path|one-path]],
[[internals/path-judgment|path-judgment]]).

**Why the guard cannot be the only enforcer.**

- *It is blind to what a child does on its own.* Once ral spawns `sh`, the guard's
  work is done; `sh` may read, write, and re-exec freely. Confining that requires
  the OS.
- *Hence the asymmetry by dimension.* A spawned child's reads and writes are held
  by the sandbox; `net` has no in-process guard at all, because ral dispatches no
  network operation for the guard to see, and neither has `gpu`
  ([[design/grant|grant]]).
- *Linux exec is a Landlock domain, the denies subtracted.* bwrap cannot
  path-filter a child's re-execs, so the payload enters a Landlock layer of its
  own inside the envelope, built by the parent. Landlock is allow-list only, so
  a deny *inside* an allowed directory is rendered as that directory's entries
  the table still admits, over the tree as it stands at launch. One gap stays
  with the in-process guard, which a child's re-exec escapes: a
  bare-name veto under a trusted write, which a child can sidestep by authoring
  a renamed copy and which is therefore not carried into the kernel. Another
  is held by neither enforcer: both judge the path passed to `execve`, not the
  code a process runs, so
  an admitted loader or interpreter handed an unadmitted file as an argument
  runs it ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]). What the
  kernel admits is one law on every platform: the guard's own evidence table,
  positives before negatives, so an exclusion beats every allowance with no
  specificity, plus *support*, the carriers and the dynamic linker a launch
  needs, joined as allowances where the author wrote none and dropped where an
  exclusion on either axis covers them
  ([[related/access-control-algebra|access-control-algebra]],
  [[decisions/261004_exec-carriers|exec-carriers]]). ral's own binary is an
  ordinary file, admitted only where the table admits it. The rules carry only
  the exclusions a layer wrote: the meet of two tables joins their exclusions
  and keeps an allowance only where both support its own subject, never writing
  a default as an exclusion, which Seatbelt's caseless match would turn on every
  spelling of the name ([[design/authority-tables|authority-tables]]). The
  kernel and the guard agree on what runs *and* what is read: both read every
  write prefix and every run place, an allowance reaches the kernel as the frozen
  real path the guard judges by, and a launch whose frozen allowance has since
  been redirected is refused rather than followed.

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
| ``net: `offline`` | `--unshare-net` | refuse: `projection_enforceable` |
| ``gpu: `withheld`` | no GPU node in the envelope's `/dev`, masks over the host's; no Landlock part | never lacking on Linux |
| `exec` — which path may be `execve`d (not which code runs), a deny inside an allowed directory included, over the tree at launch: a program added afterwards to a directory holding a block is denied until the next launch | Landlock `Execute` ruleset, the parent subtracting each block from the directory that holds it; every directory admission, `every` among them, lowered against the envelope's binds | refuse: `confinement_unavailable`, an exec opinion alone asking for the envelope |
| `exec` — a bare-name veto under a trusted write | in-process guard | the guard stands alone |
| die with parent, new session, no core, the seccomp deny-set (kills kernel attack surface; refuses mounting, user namespaces, `TIOCSTI` and `TIOCLINUX` with an errno) | bwrap + `pre_exec` | applied where possible |
| private ipc / uts | `--unshare-*` | never refused |
| `/sys/fs/cgroup` is the payload's own tree | cgroup namespace + re-rooted bind | the tree is the host's |
| no signalling the host; host process table hidden | pid namespace + fresh `/proc` | the table is the container's own |
| private ptys | `--dev` | host devpts with every terminal present at render masked; the child allocates ptys, and a terminal the host opens during the launch remains reachable for that launch |

The first row has one exception on Linux: a `deny` naming a path *absent at
render* has no mask, since making its mountpoint would touch the host. Even
under a read-only bind, the host can create the name during the launch and
that launch reads it; the next launch masks it. Under a writable bind the
child can also create the name and leave it on the host. Each launch whose
render finds it absent retains that gap for its lifetime, so the risk includes
both leaked host-created secrets and planted files. ral's own guard refuses
reads and creation throughout. macOS holds the name from the start, refusing
both reads and creation; Windows carves the allowance around it, so a name
made in a carved directory stays unreadable to that launch
([[internals/holding-a-deny|holding-a-deny]]).

The two agree on what a deny means: every spelling some filesystem takes for
its name. The guard's deny rules speak under `identity::collision_key`, its allows
under the name as stored; Seatbelt matches its canonical caseless part, all
of it on APFS; a bwrap mask and a Windows ACE hang on whatever object the
volume's own lookup finds, which on an existing path is the same thing. On a
case-sensitive volume the guard therefore over-denies relative to the Linux
and Windows kernels — a distinct `secrets` beside a denied `Secrets` is
refused in process and not to a child — an over-approximation, stated in the
refusal ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]];
every platform's case in [[design/authority-tables|authority-tables]]).

## The contract between guard and kernel

**The kernel agrees with the guard on resolved requests at launch, up to
declared support and declared limits.** The guard's law is exclusion-wins over
evidence ([[design/authority-tables|authority-tables]]); the kernel is handed
the same table, the same read region (run places included) and, for exec, the
policy joined with *support*. No backend adds an exception to an authored
exclusion: a policy that relied on a carrier, linker, exact file or deeper
directory reopening an exclusion fails, and the refusal says an allowance
cannot override an exclusion.

*Declared support* is what the kernel shows beyond the policy so a launch can
start:

- the platform floor (part of the policy), the live support set (carriers, the
  Linux linker candidates), each with the reads its loading needs;
- on Linux, the pinned bwrap and trampoline, readable at their own names;
- the base profile's ambient allowances (`macos-base.sbpl`: the root's own entry,
  the timezone database, the discard and terminal devices) and the Linux
  envelope's private `/tmp`, `/dev` and `/proc`, none of which is the host's.

*Declared limits*, each a place where guard and kernel can differ:

- **On Linux a pin is runnable wherever it is readable.** The pinned bwrap and
  trampoline are bind-mounted read-only at their own names whatever the `fs`
  grant reads, and where `exec` is unrestricted Landlock carries no exec
  ruleset, so a readable pin is also runnable. Under ``grant [fs: [read: [`cwd .]]]``
  with ral installed outside the readable set, `ral -c …` is refused by the
  guard (running reads, and ral's binary is not readable), but
  `sh -c '/path/to/ral -c …'` runs and the nested ral boots with no grants. `fs`
  and `net` stay kernel-confined; only guard-only rules are lost (verbs, and an
  exec allow-list that an unrestricted `exec` never had). Under any restricted
  `exec`, or `every` with exclusions, the pins are not admitted. See
  [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].
- **Support depends on the launch, not on the policy alone.** A support file the
  policy excludes refuses the programs that start through it, though the policy
  says nothing about them; an allowance can admit a support file the policy
  never names. The extension invariant therefore holds of policy denotations and
  not of a kernel table as a whole.
- **A nested ral** inherits the kernel confinement but not the stack: bundled-tool
  rules, verbs and whatever else only the guard enforces do not bind it, and
  under an exec allow-list that admits ral it runs every bundled tool. exarch
  excludes its own binary and no other.
- **`/proc/self/exe`.** A running program reads, and may re-execute, its own
  image. The read is why a bundled tool reads ral's binary; the re-exec runs only
  where the table admits the image.
- **Hard links.** On Linux an admission is of an inode, so an excluded path that
  is a hard link to an admitted file runs when the admitted name lies beneath a
  directory admitted whole; within a listed directory the walk refuses an excluded
  inode under every name it meets. macOS rules are by path and hold.
  Independently, a path grant does not own the bytes reachable
  through it: a hard link already inside a granted region to a file outside it
  reads or writes that file, in the guard and every kernel. Only the pinned
  binaries' inodes are protected, and the deputy findings do not see it.
- **Writing meets running.** A broad write that covers a run-admitted directory
  lets a child author programs there, including ones the user later runs outside
  any sandbox; an exclusion does not stop a copy, a newly written program or a
  script fed to an admitted interpreter. An admitted interpreter runs any script
  it can read, and the dynamic linker any ELF it can read (`ld.so ./prog`).
- **Name vetoes are advisory** beneath writable directories and beneath `Any`:
  the kernel walks a veto only beneath directories no write reaches, so that
  building and then running in one command works.
- **Landlock subtracts over the tree as it stands at launch.** Beneath `Any` a
  spine starts at `/` or at a bind root, so a program created during the launch
  in any directory from there down to an excluded path's parent does not run
  until the next launch.
- **Under `Any` with exclusions and a restricted fs**, the envelope's private
  `/tmp` holds no host object, so no admission reaches it and a program built
  there does not run. exarch meets this always, through its self layer:
  `dangerous` with a restrict file that restricts fs is the shipped case.
- **fs, as before:** on a case-sensitive volume the Linux and Windows kernels do
  not hold an exclusion under another spelling, and an absent excluded path under
  a writable prefix reaches neither kernel until the next launch.
- **Windows.**
  - An unrestricted fs projection asks for no capability and rests on the ambient
    `ALL APPLICATION PACKAGES` grants, so a child can be refused a file the guard
    permits, and ambient grants can expose files a supplied read region does not
    name.
  - A launch whose fs region holds the universal prefix is refused, though the
    guard permits what it would do: an AppContainer reaches only what is stamped
    for it, and the root would narrow to one drive.
  - A grant lives on the NTFS object, so a file renamed out of a granted tree
    keeps its grant for the tokens that held the tree.
  - A volume mounted in a folder, with no drive letter, cannot be granted: its
    mount point is a name-surrogate reparse point, indistinguishable from a
    junction, and the frozen pin refuses every allowance through one. It fails
    closed. Giving the volume a drive letter and granting that works.
  - Windows has no kernel exec layer; exec is the guard's alone.
  - Windows cannot withhold the GPU; a withheld GPU there neither confines nor
    refuses, and the summary says so.
- **The kernel sees no argv.**

**macOS has no stacking.** Landlock layers inside the envelope; Seatbelt
profiles do not stack. A process already inside one gets `EPERM` entering a
second, so a confined runner's per-command child cannot host a launch under a
restricting grant: ral cannot know the entry profile holds what the grant
promises, and a host that cannot hold a promise refuses. The launch is refused
with attribution — this lineage is already profiled — never run under the wider
profile it already has.

**macOS's process row is a budget.** There is no pid namespace to hide the
host's table; a confined child instead launches with `RLIMIT_NPROC` at the
user's process count at launch plus 512, soft and hard (`process::limits::fork_brake`;
a count that cannot be taken refuses the launch, as an unarmed Windows Job
Object cap does: a child under a grant never runs unbraked). Darwin
counts processes per real UID but compares each `fork`/`posix_spawn` with the
*forking* process's own limit, so only the confined subtree's forks are refused
and every other process of the user keeps its own. The threshold is absolute:
if the user's session grows past it, the confined command's forks fail first,
never the desktop's. It refuses forks; it does not kill. The system ceiling
and the inherited hard limit both cap the budget; an installation failure
refuses the launch. A status-128 failure with no attributed kernel denial
gets a conditional explanation of this limit, never a claim that the exit
status proves a refused fork.

**Protections engage by restriction, not by presence.** Resource limits,
process braking and loader-variable scrubbing are active when some layer
restricts: `exec` or `fs` supplied, `net` `offline`, `gpu` `withheld`, or a verb list that omits
a verb (`is_restrictive`, one predicate per axis). A neutral layer (`()`, a
full verb list) leaves ral ambient and, inside a restrictive stack, turns
nothing off; exarch's summary calls a layer ambient by the same predicate
([[decisions/261012_grant-surface-is-typed|grant-surface-is-typed]]).

**Neither enforcer is the body's to rewrite.** Every launcher pinned at boot —
on Linux both bwrap and ral's own trampoline, and ral itself wherever else it
re-execs — is closed to a confined child by its own read-only bind, and to
ral's own writes by the guard's `Guarded`
refusal: judged by inode, so a hard link names it too, and *before* any grant
is folded, since a stack with no `fs` opinion is `Unrestricted` and exactly the
case to catch. It is the discard device's twin — a name no grant needs to
mention, always yes; an inode no grant can name, always no. A deny entry could
not carry it, never being reached under an open stack; a sealed-memfd copy
would lose a setuid bwrap its bit and Ubuntu's AppArmor userns profile, which
attaches to the exec'd file's path; a content digest detects the change, not
the poison, and the restart it demands pins the poisoned bytes. What stays open
— another same-uid process, a later session — a root-owned bwrap lifts
([[internals/linux-envelope|linux-envelope]]).

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
