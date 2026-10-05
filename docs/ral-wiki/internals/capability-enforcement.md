---
verified_at_commit: 1eee86cd
verified_at_date: 2026-10-05
anchors: [check_exec, Admitted, ExecRules, Head, carriers, check_fs_op, check_fs_exact, locate, walk, Located, admits_fs_exact, fs_verdict, pinned_binary, sandbox_projection, allow_region, deny_region, GrantStack, sandboxed_command, build_command, projection_enforceable, serve_warrant, Warrant, inherit, bwrap_argv, SessionSandbox, fs_capability_name, ensure_fs_grant, deputy_prefixes, confinement_unavailable, spawn_error, Envelope, InfoFd, HostEnvelope, render_dev, render_cgroup, default_ro_binds, Pinned, register_envelope]
---

# Capability enforcement: one chokepoint, two enforcers

[[design/grant|The grant design]] states the lattice — authority attenuated by
intersection. This is how a check actually runs: an in-process decision layer and
an OS sandbox that backs it for external commands, each authoritative exactly
where the other is blind ([[design/two-enforcers|two enforcers]];
`core/src/capability/`, `core/src/sandbox/`).

**Every yes/no is a `capability::check_*(&Context, …)` that folds the whole
stack.** Each decision (`capability/enforce.rs`, `capability/sandbox.rs`) is a free
function over a borrowed `Context`, and each meets the dynamic `GrantStack`
(`ctx.grants`) before answering, so a verdict reflects authority intersected
across the *whole* stack, not a single frame:

- `check_exec`, which returns the `Admitted` token every launch demands;
- `check_fs_op`, a read *by name* — a predicate, a listing, a module load;
- `Shell::locate`, the door every open goes through, which judges the
  located object with `check_fs_exact`;
- the editor/shell bool checks;
- `sandbox_projection`, the OS-renderable `SandboxProjection`.

The `capability` module is the only place authority is decided — a module
boundary, not a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). **The fold composes
verdicts over layers; it never flattens the layers into one frame.** A
`Capabilities` is one layer and the `GrantStack` is the meet — each verdict
(`capability::exec::rules`, `allow_region`, `deny_region`, `permits_detach`)
walks the stack and combines what each layer says about *this* access. There
is no `Capabilities::meet`: an authored `ExecGrant` is not closed under
intersection, because a bare key means the file the host `PATH` finds at
check time ([[decisions/260906_object-not-name|object-not-name]]). Its
compiled form is: `rules` compiles every layer into an `ExecRules` table on
each question and meets the tables pointwise
([[decisions/261004_exec-rules|exec-rules]]); restrict as meet and
extend-base as widen, deny-overriding both ways, are read against the
literature in [[related/access-control-algebra|access-control-algebra]].
`Widen` serves the one place a union is meant — exarch's `--extend-base` widening a single base layer — with
its own "silence lifts no veto" rule.

- a dimension omitted from a layer inherits the ambient authority;
- a dimension present can only narrow;
- a deny is anti-monotonic — a later layer adds denies but never reopens a denied
  region ([[design/scoping|dynamic frames]]).

Exec is three-valued (`Verdict`: Allow / Only / Deny). The bundled coreutils and the
structured primitives route through this same chokepoint
([[internals/builtins-registry|builtins]]), which closes the bypass and lets ral
stay a [[invariants/single-binary|single binary]].

**Path matching is a fixed rule** (`core/src/path/`):

- expand sigils and `~`;
- lex;
- canonicalise (resolving symlinks);
- match by prefix (`path_within`).

Canonicalising *before* matching is why a directory scoped by a grant cannot be
escaped through a symlink or `..` — for a read *by name*. **An open takes a
fifth stage instead** (`path/walk.rs`): `Shell::locate` walks the name from
the root through *search* handles — the right to resolve names through a
directory, not to read it, so the walk asks of an ancestor exactly what the
kernel's resolver asks and a sandbox's metadata-only ancestor rule suffices —
never letting the kernel follow a symlink, splicing each link it meets into
the remaining name itself — a `..` is the kernel's, physical, climbing out
of a link's target rather than its place, and on macOS each component is
spelled as the directory stores it, so a case-variant name meets the deny
frozen under its stored spelling — and
judges the *object* it lands on (`check_fs_exact` on `Located::real`, which is
canonical by construction). The `Located` then performs the open, stat,
staging, rename and unlink relative to the directory handle with
`FollowSymlinks::No`, so what was judged is what is touched: a dangling link
is judged at its target, and a component swapped after the judgment is never
followed. `check_fs_write` no longer exists — every write is a `locate`.

**A command head is judged by the program it will run.** `Head::resolve`
(`runtime/command/head.rs`) finds a `Program` once — a bundled `Tool`, or a
host `File` with the path the launcher runs and its real path — and
`ExecRules::verdict` judges it, the one function that does
([[decisions/261004_exec-rules|exec-rules]]). A path key matches the real
path by equality and a directory by containment, the deepest deciding; a bare
key is the file the host `PATH` finds, so a planted `/tmp/evil/rg` does not
inherit `rg: allow`, and a bare deny also vetoes the name wherever it
resolves, so it stops an absolute `/bin/bash` and a symlink to it alike. A
head naming no file has no program: 127 or 126, never a denial.

The limit is worth knowing rather than discovering: **a name veto is not a
containment boundary.** A *copy* of a denied binary under another name is a
different file, carrying no trace of the name refused, and an allow dir admits
it — in the in-process guard and, since Seatbelt's `(deny process-exec (regex #"/bash$"))`
also sees only the new name, in the macOS profile too. What holds there is the
projection, not the name: the copy is spawned under the same confinement as its
author, so it reaches nothing new. `capability/deputy.rs` reports the writable
exec-admitted prefixes that make such a copy runnable — only where a grant
restricts *both* dimensions, since an unrestricted `fs` is not "everything
writable" — but it reports rather than denies, and the overlap is not itself an
escalation ([[design/grant|grant]] §Concessions,
[[decisions/261004_exec-rules|exec-rules]]).

That premise is true of the folded grant but was false of the macOS backend,
which renders an unrestricted `fs` as `(allow file-write*)`: a grant that
vetoed a command while holding no fs opinion could have its veto answered with
a copy dropped into an admitted directory. So `build_profile` freezes the exec allow-set (no writes to a
directory whose contents the profile will run) exactly when a veto meets an
unrestricted `fs`, which contradicts no layer, since none asked to write there.
Where `fs` *is* restricted the overlap stays a stance somebody took, reported
and not denied: `reasonable` admits `cwd:/` for the very scripts it lets the
agent write, while `edit-only` and `read-only` admit no directory they can
write, which is what lets their `git`/`bash` denies mean something.

**The in-process guard covers what ral dispatches; the OS sandbox covers what a
spawned process does on its own.**

- *Exec* — guarded in-process on every platform: `check_exec` judges the
  program and its argv *before* the spawn and returns `Admitted`, the token
  `build_command` launches from. On macOS the Seatbelt profile additionally
  renders the rules as `process-exec` forms, catching re-execs the in-process
  guard never sees (`sh -c`, `find -exec`); on Linux a Landlock domain entered
  inside the bwrap envelope carries their allows into the kernel, Landlock
  being unable to subtract inside an allowed directory, so on Linux an exec
  deny or veto under an allowed directory is not yet enforced by the kernel
  layer ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]); the
  AppContainer on Windows has no path-exec filter, so there the in-process
  guard stands alone and check-to-exec timing stays open. The kernel's list is
  `ExecRules::kernel` of the very table in the `Admitted`, ordered by `Rank`
  so last-match-wins is the guard's precedence, with the carriers spliced in
  ([[decisions/261004_exec-carriers|exec-carriers]]): guard and kernel cannot
  disagree, except that the kernel sees no argv and no bundled tool's name.
- *Filesystem* — guarded in-process too (`check_fs_op`, read and write), and
  backed by an OS sandbox that confines a spawned child's own reads and writes:
  Seatbelt on macOS, bwrap on Linux, an AppContainer LowBox token on Windows.
  Guard and profile read *one* fold (`capability/fs.rs`: `allow_region` meets a
  region across layers, `deny_region` unions it), so here the conservatism
  invariant needs no differential test — the two cannot disagree. All that
  separates them is when the fold runs: afresh on every check for the guard,
  once at spawn for the profile, because that is when the profile is written.
  The projection carries no allow beneath a deny (`PrefixSet::outside`):
  under deny-wins such an allow is dead, and a backend whose primitive orders
  explicit allows before inherited denies — a Windows ACL — must never be
  handed one. On macOS the directories a deny needs kept in place
  (`FsRules::pinned_dirs`) are the ancestor closure of every *rendered* deny
  name within a rendered write name, so an alias's chain is pinned alongside
  its target's ([[internals/seatbelt-profile|seatbelt-profile]]).
  One target is excused before either region is consulted: the *discard
  device* — `/dev/null`, or `NUL` on Windows — which `ResolvedPath::is_discard`
  names on either host, and which needs no authority because nothing reaches
  the disk through it. `GrantStack::admits_fs` deliberately does not share the
  exemption: it decides *membership* in a region, and a device excused from an
  access is not thereby a member of anything. The same predicate settles
  whether the act is a fact at all ([[design/audit|audit]]). Its twin on the
  other side is the one write no grant admits: `fs_verdict` answers `Guarded`
  for a write onto a binary the sandbox pinned at boot — bwrap on Linux, ral's
  own executable — *before* the stack is folded, because a stack with no `fs`
  opinion is `Unrestricted` and exactly the case to catch. `sandbox::pinned_binary`
  judges by inode, so a hard link or a rename since boot names the pin too,
  and a replace-by-rename stays admitted: the pinned copy survives it
  ([[design/two-enforcers|two-enforcers]]).
- *Network* — no in-process guard at all, since ral dispatches no network
  operation itself, so the OS sandbox is the sole enforcer; on Windows the
  enforcement is the withheld network capability SIDs — a LowBox token
  without them cannot open a socket.

**A `deny` must survive the filesystem moving around it, not just name a path
to refuse.** [[design/grant|grant]] states the invariant: no confined child can
cause a denied path's contents to become reachable under a name the deny does
not cover. Writing, unlinking, or hard-linking a `deny_paths` entry itself is
already blocked — the Seatbelt profile renders a `subpath` deny for each — but
an *ancestor* of that entry sits outside its subpath and inside the write
prefix's own allow, so a confined `mv` or `rm` could relocate the ancestor
directory and carry the denied bytes to a name nothing covers.
`SandboxBindSpec::pinned_dirs` (`core/src/types/capability.rs`) closes the
gap: every proper ancestor of a `deny_paths` entry that lies within some write
prefix — the write prefix root included — is collected, over both a deny's
surface spelling and its symlink-resolved target, so a symlink swapped in
after sandbox entry is covered too. `build_profile`
(`core/src/sandbox/macos.rs`) emits `(deny file-write-unlink (literal
"<dir>"))` for each pinned directory, after the write prefix's own covering
allow (Seatbelt is last-match-wins); `literal`, never `subpath`, is what keeps
a pinned directory's *entries* mutable — only its own name-in-parent is
frozen. The price lands on macOS specifically: a grant that writes a repo and
denies `.git/config` also refuses `mv .git .git.bak` and `rmdir .git`, since
both `.git` and the repo root are pinned ancestors of the denied entry.

**Linux renders no pin, and that is not an oversight.** bwrap realizes a
`deny` as a mount laid over the denied path (`DenyMask`,
`core/src/sandbox/linux.rs`); a mount is anchored to the inode it covers, not
to the path string that named it at mount time, so renaming a non-mountpoint
ancestor carries the mask along and it keeps covering the real file at its new
location, while renaming or removing the mountpoint itself fails with `EBUSY`.
The invariant already holds by construction, so Linux pays a narrower price
than macOS: only the denied path's own name is frozen, and every ancestor —
the write prefix root included — stays freely renameable and removable.

**The shape of the denied path forces which mount, and the cost of confusing
them is the launch.** `--tmpfs` mkdirs its own mountpoint, so over an existing
regular file it dies with `ENOTDIR` before bwrap execs anything — a deny that
denies nothing because nothing runs. Hence `DenyMask::over` as the only
constructor: an existing non-directory takes `--ro-bind /dev/null`, bound
without `MS_DEV` and so unopenable either way; a directory takes `--perms 0000
--tmpfs`, whose missing bits refuse the owner as much as anyone, so a tool
writing there is refused rather than told it succeeded. That tmpfs is the
sandboxed uid's own, so a child that deliberately `chmod`s the bits back has
private scratch at the name — memory the host never sees, never the denied
directory, and the mask's stated limit
([[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]).
Nothing mounts over a symlink, so a
symlinked `deny` is masked at the resolved target `sandbox_projection` carries
beside the surface spelling.

**A name that does not exist gets no mask at all.** Every mount bwrap could lay
there it must `mkdir` a mountpoint for first, and the writable binds are
identity binds of real host directories, so that mkdir is a write to the host —
`EROFS` and a dead envelope under a read-only bind, and under a writable one a
deny that creates the very name it forbids
([[decisions/260905_an-envelope-does-not-touch-the-host|an-envelope-does-not-touch-the-host]]).
Under a read-only bind nothing is lost, creation being the only access an
absent name has. Under a writable bind the deny falls to the in-process guard
alone, which is a Linux seam and named as one: macOS's rules are negative and
range over names, so Seatbelt enforces the same deny in full. The masks that do
land refuse with `EACCES` against macOS's `EPERM`, so a cross-platform test
should assert the bytes are unreachable rather than an errno — and because the
failure modes here are a sandbox that never launches and a mount that lies,
they are pinned by tests that spawn the envelope for real
(`sandbox::linux::tests::a_denied_path_refuses_every_access_while_the_body_still_runs`,
`::building_an_envelope_never_creates_a_denied_name_on_the_host`).

**Where the host refuses bwrap a mount, the envelope rebuilds what the mount
would have provided.** A rootless container's mount layer will not hand bwrap a
fresh devpts, so `--dev` dies in setup and no `Restricted` envelope launches
there at all; the same runtimes mask `/proc`, and the kernel then refuses the
fresh procfs a pid namespace needs. `HostEnvelope` learns both with one probe
spawn each (`sandbox/linux/host.rs`): `render_dev` lays out `--dev`'s own shape
by hand where it must — a tmpfs, the device nodes bound in, the `pts/ptmx`
symlink, a fresh `/dev/shm` — and `--unshare-pid` is emitted only where the
table can be hidden. What cannot be rebuilt is reported, not refused: neither
is a restriction a grant names, so `RAL_DUMP_SANDBOX_PROFILE` prints the
`HostEnvelope` naming each unheld invariant, its cause and the flag that lifts
it, and the child sees what it would have seen unconfined — the container's
own table, the host's `/dev/pts` ([[design/two-enforcers|two-enforcers]]). The
render is a pure function of the probed `HostEnvelope`, so the argv tests
assert both shapes from literals rather than from whichever host runs them.
Such hosts still refuse the read-only rebind of their own locked `/etc/hosts`
and `/etc/resolv.conf`, which remains open.

**A kernel pseudo-filesystem inside the envelope describes the envelope; one
that cannot is a host bind like any other, present only where something needs
it.** `--proc /proc` is fresh with the pid namespace. bwrap has no sysfs op,
and a bound `/sys` is the *mounter's*: its `class/net` lists the host's
interfaces whatever `--unshare-net` did, its `class/dmi` and `bus` name the
machine. So `default_ro_binds` narrows `/sys` as it narrows `/etc`, to what
sizes a program — `devices/system/cpu`, `kernel/mm/transparent_hugepage` — and a
grant that wants the rest reads `/sys` by name. `/sys/fs/cgroup` is the case
that is *wrong* rather than revealing: under the cgroup namespace a child's
`/proc/self/cgroup` reads `0::/`, and a runtime joining that onto the host's
tree reads the root's limits, which are none. `render_cgroup` re-roots the tree
on ral's own cgroup — the namespace's root, what a fresh cgroup2 mount inside it
would show — on both projections, over the projection's binds; where the host
builds no cgroup namespace (`HostEnvelope::private_cgroup`) the host's tree is
the true one and is bound as it is, and the dump says so
([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).
Pinned by `sandbox::linux::tests::sys_is_narrowed_to_what_sizes_a_program_and_the_cgroup_tree_is_the_payloads`
and `::a_confined_program_reads_its_own_cgroup_and_none_of_the_hosts_interfaces`.

**The envelope's identity is fixed at boot, and nothing a session does can
change it.** The launcher a confined command runs under is exactly as trusted
as the file it is, so that file is never chosen by name at spawn time — where
the only environment in scope is the one built for the payload, `PATH`
override included, and `Command::new("bwrap")` would have let a `within [env:
[PATH: …]]` or a planted binary earlier on `PATH` supply code that runs before
any confinement. Instead `early_init` pins bwrap (`linux::register_envelope`)
on the absolute entries of the `PATH` ral was started with, before any shell
exists, as a `reexec::Pinned` — the same fd-pin ral uses for its own re-exec —
and every launch execs `/proc/self/fd/N`, so neither a `PATH` override nor a
replace-by-rename at the pinned path reaches it. In-place rewriting of the
pinned inode — the one change a pin by descriptor cannot see — is closed on
both sides of the dispatch boundary: for a confined child by the envelope
itself, `bwrap_argv` read-only binding the envelope's own file
after the projection's binds, `/` wholesale under `Unrestricted` included, and
before the masks, the Linux twin of macOS's `freeze_admitted_set`; for ral's
own writes by the `Guarded` verdict above. What stays open is another same-uid
process, and any session after this one — a session cannot vet the enforcer it
boots on — so where the pinned bwrap is writable by ral's uid the profile dump
says so and names the remedy, a root-owned bwrap. A host with no bwrap to pin
is reported by `linux::envelope` at the first launch that needs it, through
`confinement_unavailable`; `Launch::envelope` names the binary for a spawn
failure's wording only. Pinned by `sandbox::linux::tests::the_launcher_is_the_pinned_envelope_and_never_a_name`,
`::the_envelope_binary_is_read_only_inside_every_envelope` and
`capability::enforce::tests::a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted`.

**The sandbox is applied per external command, not by re-execing the grant
body.** A `grant` is a *local* dynamic effect scope: its body evaluates in
process, and `transport::dispatch` just runs that body locally — nested grants
compose by intersecting authority on the evaluator's `GrantStack`, which is not a
process boundary ([[design/grant|grant]]). Confinement happens one level down, at
external dispatch. When `build_command` (`runtime/command/process.rs`) spawns an
admitted external or bundled child under a restrictive projection, it routes
through `sandboxed_command` (`sandbox/launch.rs`), which confines that *one*
child:

- *Linux* wraps each child in `bwrap` via `make_command_with_policy`, threading
  the logical cwd in as `--chdir`; a cwd the grant does not cover gets an empty
  `0555` tmpfs stand-in laid before the binds (which hide it where they do
  cover it), so a grant narrower than the cwd still runs commands, as under
  Seatbelt, and a relative write there is refused. The envelope is the child's whole world, not
  only its filesystem view: its own ipc, uts and cgroup namespaces on every
  projection, and a pid namespace with a fresh `/proc` wherever the host can
  build one, so a confined child sees and can signal nothing of the host,
  `/proc/self` is its own, and `ps` under any grant shows the envelope alone.
  bwrap never execs the payload in place — a monitor clones a namespace init,
  which forks the payload — so a `Kept` launch reads the payload's pid back
  over `--info-fd` (`InfoFd`, a socketpair, which no same-uid process can
  reopen through `/proc` to forge a `child-pid`); that pid, a session leader by
  `--new-session`, is the group every ladder signals, and the monitor is
  addressed by nobody.
  `Launch::spawn` places an enveloped launch `NewLeader` whatever was asked and
  returns the payload's group, `ForegroundDecision` never hands an envelope the
  terminal, and a pipeline collector addresses each confined stage's envelope
  beside its own group ([[internals/pipeline-execution|pipeline execution]]);
- *macOS* re-execs a tiny launcher, `ral --warrant`: the Seatbelt profile,
  compiled in the parent, and the program to become — a host file, the absolute
  path the guard judged, or a bundled tool — arrive in its warrant, and
  `serve_warrant` enters Seatbelt from the profile alone and then `execve`s
  exactly that path;
- *Windows* attaches the projection's AppContainer LowBox
  `SECURITY_CAPABILITIES` to the child's own `CreateProcessW`
  (`windows::session::confine`), so the parent's spawn is the confinement
  point — no re-exec child.

**The confined child is handed what to enter, and what to become, on a
descriptor.** Both Unix backends re-exec ral as `ral --warrant`, whose whole
argv names nothing: the confinement and the program arrive in a `Warrant` the
parent compiled, on fd 99. The warrant crosses on a channel no other process
can write — on Linux a sealed memfd, since a same-uid process may reopen any
descriptor through `/proc` and a sealed copy is as read-only as ours, read back
and checked once sealed; on macOS, which has no `/proc`, the read end of a
socketpair whose writer closes before the child exists. The child refuses an
unsealed memfd, a non-socket, and a warrant not in canonical form: decoding
re-encodes and compares. On Linux the real argv is `bwrap --args 98 -- <ral>
--warrant`: `bwrap_argv` is pure and returns bwrap's options alone, which bwrap
takes from `--args`. The descriptors a confined launch hands down — bwrap's
`--args`, the warrant, `--info-fd`, the seccomp programs, the Landlock admits —
sit at fixed slots defined once, and one `inherit` places them all; the child
closes the range once it is confined, so the program inherits none.
`serve_warrant` runs before `early_init`, so the trampoline pins and opens
nothing ahead of its confinement. Nothing runs unconfined: a failure before
the program starts exits 126, a missing program 127; an `execve` refusal takes
its code from the same `SpawnFailure` the in-process spawn uses.

*Failing closed is the handoff's whole stance.* The warrant's `Confinement`
(`Seatbelt(profile)` on macOS, `ExecAdmits` on Linux) is the one thing the child
enters, and `Warrant::confine` is the only road to the program — after the
confinement is entered and the handoff closed — so no code path runs a program
unconfined. On Linux the parent's `prepare` refuses a restricting exec grant
where Landlock is unavailable, a file admit that has become a directory is
dropped rather than widened to a hierarchy, and the child's `enter` refuses if
the warrant promised exec rules and finds no Landlock. A sandboxed launch needs
ral's own pin (`reexec::own`) and re-verifies it (`Pinned::verify`) before
issuing a warrant; unpinned, it refuses.

On Windows filesystem authority is *path*-keyed, and the token selects. Each
`(canonical path, kind)` grant derives a deterministic capability SID from a
hash of the canonical path; its ACE is stamped once, ever, and never reverted,
and `session::confine` mints into the child's token exactly the capability SIDs
its projection names. The kernel-level check therefore enforces the same
projection the in-process guard judges — a narrowed grant or a subagent's
narrowed permissions hold at the OS layer, because the narrower token does not
carry the wider paths' capabilities. Persistence is safe because a capability
SID is evaluated only in the AppContainer pass of the access check, whose result
intersects the normal user pass: an ACE no live token names is inert and can
never widen a process's reach past the owning user's own. A detached worker
therefore keeps the authority it was born with by construction. The residual is
that an ACE lives on the NTFS object while a grant rule names a path, and
Windows does not re-inherit on a same-volume rename: a file moved into a granted
tree stays dark, and one moved out of an rw tree keeps that tree's capability, so
path-based rules and object-sticky stamps agree only while the tree is still
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).

The launcher pins the *current binary* (`SANDBOX_SELF`, fixed at `early_init`) so
an on-disk swap cannot subvert it. Because confinement is per-command, it
engages only when a child is actually spawned: a `grant [net: false] { … }` with no
external child does not fail closed, and an offline request on a backend without
kernel network enforcement fails closed at the spawn (`projection_enforceable`).

Confinement can also be unavailable for a reason no pre-flight can see. On Linux
the envelope is `bwrap`, a host package rather than part of ral, and a host
without it fails closed at the spawn with `ENOENT`. The subtlety is *whose*
`ENOENT` it is: `vet` has already resolved the target on `PATH`, and an envelope
execs its target itself, reporting a missing one as an exit status. So under an
active projection a spawn-time `NotFound` can only be the envelope's, and
`spawn_error` says so — reading it as the target's would accuse the one program
known to exist, and make a host lacking bubblewrap look like a grant that denies
everything. A launch therefore carries the envelope it execs
(`Launch::confinement`), set by the one backend whose envelope is a separate
binary, so blame is read off what the launcher did rather than re-derived from
the shell's state. Both routes end in the same refusal,
`sandbox::confinement_unavailable`: nothing ran, and the sandbox is why.

A ral-written pipeline stage is unrelated to this sandbox re-exec: it runs on
its own thread of the parent process, sharing the parent's memory directly,
never a re-exec'd child of any kind
([[internals/pipeline-execution|pipeline execution]];
[[decisions/260902_stages-are-threads|stages-are-threads]]).

**The hard rule for any synchronous child wait a host still performs: it must
own an out-of-band cancellation path.** A parent blocked reading a framed
response cannot observe its own foreground `CancelScope` by cooperative
polling — the poll never runs while the read is parked. Deadline and Esc
therefore cannot break a wedged wait unless the parent has a side channel that
signals the confined child subtree from outside it. Extra signal authority
*inside* the child is not a substitute: it lets a child signal its own
descendants, but it does nothing to free a parent stuck on the IPC edge.

A bundled coreutil's filesystem access has no in-process guard, so under a
restrictive grant it is never inlined: it is spawned as a re-exec child of ral — `--ral-bundled-tool
<tool>`, or a warrant naming the tool — that receives the same per-command sandbox as any external, which
is what floors it
([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]];
[[decisions/260617_sandbox-external-children|sandbox-external-children]]).

This is the boundary [[design/exarch-architecture|exarch]] reuses unchanged — an
agent run is a host-pushed grant frame over this same stack.

See also [[design/grant|grant]],
[[design/capability-carriers|capability-carriers]] (why the rule, the live
judgment, and the `SandboxProjection` are distinct, not one); map
[[map/core/capabilities|capabilities]]. `docs/SPEC.md` §12.
