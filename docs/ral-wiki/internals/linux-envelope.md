---
generated_at_commit: 773daa1062bb84e64c799cf502a0a5288c173f20
verified_at_commit: 646bff0a
verified_at_date: 2026-10-13
anchors: [Binds, render_objects, bwrap_argv, bwrap_command, Slot, HostEnvelope, render_dev, cgroup_tree, floor, Pinned, pin_envelope, InfoFd]
---

# The Linux envelope

**On Linux every confined command runs inside a bwrap *envelope*: a private
view of the filesystem and a private set of namespaces, built for that one
command.** This page says what the envelope mounts, what it shows the child, and
how it copes with hosts that refuse parts of it. How a command enters it is
[[internals/confined-launch|confined-launch]]; how its denies hold is
[[internals/holding-a-deny|holding-a-deny]].

## The envelope is the child's whole world

**`bwrap_command` wraps each child in bwrap, and the envelope covers more than
the filesystem**
([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).

- **Namespaces.** Every projection gets its own ipc, uts and cgroup namespaces.
  Wherever the host can build one, it also gets a pid namespace with a fresh
  `/proc`. A confined child then sees and can signal nothing of the host,
  `/proc/self` is its own, and `ps` under any grant shows only the envelope.
- **Working directory.** The logical cwd goes in as `--chdir`. A cwd the grant
  does not cover gets an empty `0555` tmpfs stand-in, laid before the binds,
  which may hide it; bwrap's `--chdir` then judges the name. `/` and the
  scratch `/tmp` are the envelope's own writable memory and need none. So a
  grant narrower than the cwd still runs commands, as under Seatbelt, and a
  relative write in a stand-in is refused. A cwd at or beneath a deny is
  refused before bwrap runs, since no mask can be entered.
- **The payload's group.** bwrap never execs the payload in place: a monitor
  clones a process that calls `setsid` (`--new-session`) and then, as the
  pid namespace's init, forks the payload, or without one becomes it. So a
  `Kept` launch reads that process's pid back over `--info-fd` (`InfoFd`, a
  socketpair), and its group is the one every signal ladder targets. No
  same-uid process can reopen a socketpair through `/proc` to forge a
  `child-pid`. Nothing addresses the monitor.
- **The group is named only once it exists.** bwrap writes `child-pid`
  before that `setsid`, when a signal to the group would be lost. The
  payload holds a second copy of the writer, which bwrap does not name, until its
  handoff sweep, so the read ends only once the group exists or the
  envelope has died, and `Launch::spawn` returns no sooner.
- **Process groups.** `Launch::spawn` makes an enveloped launch `NewLeader`
  whatever was asked, and returns the payload's group. `ForegroundDecision`
  never hands an envelope the terminal. A pipeline collector addresses each
  confined stage's envelope beside its own group
  ([[internals/pipeline-execution|pipeline execution]]).

## bwrap mounts what ral opened, never a name

**`Binds::open` plans every bind a launch shows, opens each object once without
following a symlink, and lends a copy per bind**
([[decisions/261006_the-envelope-mounts-by-handle|the-envelope-mounts-by-handle]]).
`bwrap_argv` emits `--ro-bind-fd` or `--bind-fd` from the same vector. The binds
are: the read-only defaults, the read region (`shown`: every read scope of the
fs projection, run places and live support among them, written prefixes
included), the pins of bwrap and the trampoline, bound read-only at their own
names before the masks, and, over everything, the cgroup tree. The launched
image is not a bind of its own: the running-reads law puts it in the read
region, and the table admits it as it admits any file.
Every allowance is bound as the resolved position the guard judged, opened
without following a link (`open_nofollow`) and never resolved again; the
spelling is only the destination at which the opened object is bound.

- **A path is its canonical object plus the spellings that reach it**
  (`render_objects`). `/bin` shows `/usr/bin`'s handle, except inside another
  bind, where the host's own link already stands.
- **Redundant binds are dropped.** A read-only bind inside another bind adds
  nothing. A writable bind is dropped only inside a writable one.
- **A redirected allowance refuses the launch.** Every bind object opens with
  `openat2(RESOLVE_NO_SYMLINKS)`, so a symlink put anywhere on a frozen
  allowance's path refuses the launch, naming it, rather than following it.
- **There is no write freeze.** No bind is laid read-only over an admitted
  directory a write covers; the protection it gave is a declared limit
  ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
- **Missing objects are dropped, changed ones refused.** An absent object drops
  its binds at the open, and so does a system default this user may not reach.
  A name that now reaches another object, or an open through a symlink, refuses
  the launch and says which.
- **Every deny is masked at every name a bind shows it.** A deny that would hide
  `/proc`, which bwrap execs the trampoline through, is refused.
- **An unusable bwrap refuses in its own words.** A bwrap that builds no
  envelope refuses every confined launch, and one without `--ro-bind-fd` is
  named with its version.

Pinned by `authority::sandbox::linux::tests::a_bind_is_opened_for_each_name_no_other_bind_shows`,
`::a_prefix_whose_component_became_a_symlink_since_render_is_refused`,
`::every_bind_is_a_handle_by_slot_in_layer_order`,
`::a_write_through_a_link_is_masked_at_both_its_names` and, spawning,
`::a_prefix_swapped_for_a_symlink_after_the_open_shows_what_was_opened`.

## The exec layer is lowered against the binds

**Landlock rules attach to host objects opened before bwrap starts, and a walk
from a file inside the envelope meets only the inodes above it in the
envelope.** So the bind plan, with its objects, is computed before the ruleset
and passed to `landlock::build`.

Inside a restricted envelope a bind's root has the envelope's own directories
above it, so a rule on a host directory above a bind root is off every walk
inside that bind (the payload's `REFER` admission already worked around this for
the envelope's root). `enveloped` once built the ruleset before `bwrap_command`
opened the binds; the deduped shown roots, with their objects, are now computed
first, and opening stays where it was.

- **A directory admission `D`** puts a rule on `D` itself where some bind root's
  object lies at or above `D`, and a rule on every bind root whose object lies
  inside `D`: an alias (`/alias` bound to `/tools/sub` under `Dir(/tools)`) and
  a canonical sub-bind (a ceiling that reads only `/tools/sub`). Exclusions are
  subtracted along their spines from each root. Either way the walk from
  `/tools/sub/prog` meets `/tools/sub` and the envelope's own directories, never
  `/tools`; a nested grant that runs `Dir(/tools)` under a ceiling that reads only
  `/tools/sub` cannot widen reading, so `/tools` is not shown.
- **`Any` is the directory `/`.** Under an unrestricted fs it is a rule on `/`;
  under a restricted fs, a rule on every bind root remaining after the
  redundant-bind dedup. When every exclusion is a name veto, nothing is
  subtracted, so no exec ruleset is built; signal scoping stays.
- **Linker candidates are support**, admitted unless excluded, not before the
  table; ral's pinned inode is no longer admitted at all.
- **Limits, declared:** a program created during a launch on the spine of an
  excluded path runs from the next launch; under `Any` with exclusions and a
  restricted fs the envelope's private `/tmp` holds no host object, so a program
  built there does not run; an excluded path that is a hard link to an admitted
  file runs, since Landlock admits an inode.

## Where the host refuses a mount, the envelope rebuilds it

**Some hosts refuse bwrap a mount it needs. The envelope then builds the same
result by hand, or states what it cannot build.**

- **A fresh devpts.** A rootless container's mount layer will not give bwrap
  one, so `--dev` fails in setup and no `Restricted` envelope could launch.
  `render_dev` lays out `--dev`'s shape by hand instead: a tmpfs, the device
  nodes bound in, the `pts/ptmx` symlink, and a fresh `/dev/shm`.
- **A fresh procfs.** The same runtimes mask `/proc`, and the kernel then
  refuses the procfs a pid namespace needs. `--unshare-pid` is emitted only
  where the table can be hidden.
- **`HostEnvelope` learns both** with one probe spawn each
  (`authority/sandbox/linux/host.rs`).
- **What cannot be rebuilt is stated, not refused.** Neither is a restriction a
  grant names, so the launch runs, and the child sees what it would see
  unconfined: the container's own process table, the host's `/dev/pts`
  ([[design/two-enforcers|two-enforcers]]). Running the container
  `--privileged` lifts both.
- **A pty by hand is the host devpts's.** `/dev/ptmx` opens where that
  devpts's `ptmxmode` admits: a container runtime's own devpts (`0666`), not
  the kernel's default nor a systemd host's (`000`). The host's `/dev/ptmx`
  node cannot stand in: the kernel finds a node's devpts at the `pts` beside
  it in the same mount, and a bind of one file has none.
- **The render is a pure function of the probed `HostEnvelope`.** So the argv
  tests assert both shapes from literals, not from whichever host runs them.
- **Open:** such hosts still refuse the read-only rebind of their own locked
  `/etc/hosts` and `/etc/resolv.conf`, by descriptor as by name.

Pinned by `authority::sandbox::linux::tests::a_host_that_refuses_dev_gets_the_same_shape_by_hand`
and `::the_by_hand_dev_serves_a_confined_body`.

## Kernel pseudo-filesystems describe the envelope

**A pseudo-filesystem inside the envelope must describe the envelope. One that
cannot is a host bind like any other, present only where something needs it.**

- **`/proc`** is mounted fresh (`--proc /proc`) with the pid namespace.
- **`/sys`** has no bwrap operation, and a bound `/sys` describes the host: its
  `class/net` lists the host's interfaces whatever `--unshare-net` did, and its
  `class/dmi` and `bus` name the machine. So the Linux `floor` narrows `/sys`, as
  it narrows `/etc`, to what sizes a program: `devices/system/cpu` and
  `kernel/mm/transparent_hugepage`. A grant that wants the rest reads `/sys` by
  name.
- **`/sys/fs/cgroup`** would be *wrong*, not just revealing. Under the cgroup
  namespace a child's `/proc/self/cgroup` reads `0::/`, and a runtime that joins
  that to the host's tree reads the root's limits, which are none. `cgroup_tree`
  re-roots the tree on ral's own cgroup, the namespace's root, which is what a
  fresh cgroup2 mount inside would show. It does so on both projections, over
  the projection's binds. Where the host builds no cgroup namespace
  (`HostEnvelope::private_cgroup`), the host's tree is the true one and is bound
  as it is.

Pinned by `authority::sandbox::linux::tests::sys_is_narrowed_to_what_sizes_a_program_and_the_cgroup_tree_is_the_payloads`
and `::a_confined_program_reads_its_own_cgroup_and_none_of_the_hosts_interfaces`.

## The envelope binary is fixed at boot

**The launcher is exactly as trustworthy as the file it is, so ral never picks
that file by name at spawn time.** At spawn, the only environment in scope is the
payload's, including any `PATH` override. `Command::new("bwrap")` would let a
`within [env: [PATH: …]]`, or a planted binary earlier on `PATH`, run code before
any confinement.

- **`authority::sandbox::boot` pins bwrap before any shell exists** (`authority::sandbox::linux::pin_envelope`).
  It searches the absolute entries of the `PATH` ral started with, as directories
  rather than a re-joined string (`path::locate_in`), and keeps the
  result as a `reexec::Pinned`, the same fd pin ral uses to re-exec itself.
- **Every launch execs `/proc/self/fd/N`.** Neither a `PATH` override nor a
  replace-by-rename at the pinned path reaches it.
- **In-place rewriting of the pinned inode**, the one change a pin by
  descriptor cannot see, is closed on both sides:
  - **for a confined child**, by the envelope. `Binds::open` lends both pins,
    bwrap's and ral's, as `Over` binds, each read-only by its own descriptor at
    the name its inode holds now (`Pinned::names`). They go after the
    projection's binds, `/` wholesale under `Unrestricted` included, and before
    the masks. A write gets `EROFS`, a hard link `EXDEV`, and a rename or unlink
    of the mountpoint `EBUSY`. This is the Linux twin of the write-deny macOS
    lays on ral's own file in every profile
    ([[internals/seatbelt-profile|seatbelt-profile]]);
  - **for ral's own writes**, by the `Guarded` verdict
    ([[internals/path-judgment|path-judgment]]).
- **An unlinked pin gets no bind.** A replace-by-rename took its name, so there
  is no name to lock.
- **Still open:** another same-uid process, and any later session, since a
  session cannot vet the enforcer it boots on. Where the pinned bwrap is
  writable by ral's uid, the remedy is a root-owned bwrap (the distro package).
- **A host with no bwrap** is reported by `linux::envelope` at the first launch
  that needs one, through `Refusal::Unavailable`. `Launch::envelope` names the
  binary only for the wording of a spawn failure.

Pinned by `authority::sandbox::linux::tests::the_launcher_is_the_pinned_envelope_and_never_a_name`,
`::the_envelope_binary_is_read_only_inside_every_envelope`,
`::a_pinned_binary_inside_a_writable_prefix_is_locked` and
`authority::guard::enforce::tests::a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted`.
