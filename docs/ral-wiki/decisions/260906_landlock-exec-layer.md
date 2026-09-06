---
status: active
---

# Kernel exec confinement on Linux is a Landlock layer inside the envelope

Closes [[decisions/260530_linux-exec-confinement|linux-exec-confinement]]. A
confined payload now enters a Landlock domain of its own
(`core/src/sandbox/linux/landlock.rs`) rendering `ExecProjection::Restricted`,
so a descendant's `sh -c` or `find -exec` is gated by the kernel and not only
by ral's dispatch.

**The layer goes on inside bwrap, entered by the payload itself.** A Landlock
domain that handles any filesystem right forbids `mount(2)`, so a layer applied
before the envelope kills bwrap at its first act (measured: `bwrap: Failed to
make / slave`). The order is therefore bwrap, then the payload's own
`restrict_self`, and never the reverse.

Four findings shape it.

1. **Landlock is allow-list only.** It cannot express "everything but this", so
   the filesystem half stays with bwrap's mounts, and on the exec half
   `deny_paths` / `deny_dirs` / `deny_basenames` render nothing: a deny outside
   every admit is already absence, and a deny *inside* an admit stays with the
   in-ral gate. That is a declared asymmetry against macOS, whose Seatbelt
   profile carries `deny_basenames` into the kernel.

2. **A layer restricts only what it handles — except `Refer`.** Any domain
   refuses *every* cross-directory rename and link with `EXDEV` unless it both
   handles `LANDLOCK_ACCESS_FS_REFER` and grants it, which under ABI 1 no
   domain can, the right not existing there to grant. From ABI 2 it does, so
   the layer handles `Execute | Refer` and grants `Refer` on `/` to restore
   them — restored, not restricted, though non-escalation still gives `EXDEV`
   for a rename or link that would gain the file rights its new parent has and
   its old one did not, such as moving an unadmitted file into an
   `Execute`-admitted directory.

3. **`execve` of a dynamic binary needs `Execute` on the binary and its
   `PT_INTERP` only.** The shared libraries the loader then maps need no right
   from this layer. The platform base is therefore the linker files themselves
   — `ld*.so*` under the `/lib` and `/usr/lib` variants and one level down for
   Debian multiarch — and never a command directory, since `/usr/bin` in the
   base would be a layer that denies nothing. A missing linker shows up as
   `EACCES` on 7.1, not `ENOENT`. Admitting the loaders bounds what the layer
   is: a gate on which paths may be `execve`d, never on which code a process
   runs. An admitted loader handed an unadmitted binary as an argument
   (`ld-linux-x86-64.so.2 ./payload`), an admitted interpreter handed a script
   by path, and `mmap(PROT_EXEC)` of file contents all run code the layer
   never sees, and only an fs layer handling `ReadFile` — costed and declined
   below — would see them.

4. **The signal scope is an envelope invariant, not a projection.**
   `LANDLOCK_SCOPE_SIGNAL` (ABI 6, Linux 6.12) is applied on every launch,
   including an unrestricted exec projection, and gives `EPERM` when a confined
   child signals a host process. That closes the same-uid `kill` hole on hosts
   where the container runtime denies a pid namespace, with no namespace
   needed.

Two floors, both probed by the syscall rather than read off `uname`: ABI 1 for
exec confinement — where a confined payload also loses cross-directory rename
and link, `Refer` being ungrantable there — and ABI 6 for the scope. A kernel
with no Landlock at all (`ENOSYS`, `EOPNOTSUPP`) enters nothing and
`HostEnvelope` reports each invariant as unheld; any other errno from the probe
is a host that cannot be read rather than one without the feature, and refuses
the launch with `confinement_unavailable`.

A caution for anyone reconsidering an fs layer here: once any layer handles
`ReadFile`, `execve` needs `ReadFile` on the binary *and* the loader needs it on
the whole library closure (measured) — a far larger rule set than exec alone.
Cost is not the obstacle: rules run about 1 µs each with no ceiling observed at
20 000.

See also [[map/core|core]], [[design/grant|grant]],
[[design/two-enforcers|two enforcers]].
