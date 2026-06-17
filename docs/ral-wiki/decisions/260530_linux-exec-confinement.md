---
status: open
---

# Linux exec confinement is a known gap

On Linux the OS-level sandbox is a `bwrap` envelope with two mechanisms:

- a mount namespace attenuates the filesystem;
- on x86-64 / aarch64 a seccomp-BPF filter kills a fixed set of dangerous
  syscalls (`ptrace`, `kexec_load`, `bpf`, …) while allowing everything else
  (`core/src/sandbox/linux.rs`).

This filter is a coarse syscall blocklist, **not** a path-aware policy.

Consequently, path-scoped *exec* confinement is not enforced at the kernel level
on Linux the way macOS Seatbelt profiles enforce it. ral's in-process capability
check still gates exec by name and path at dispatch ([[design/grant|grant]]), but a
descendant process started inside the envelope is not held to per-path exec
rules by the kernel.

The principled fix is landlock 6.7+'s scoped-exec, or a path-aware seccomp
filter. Neither is wired up. This is **deferred, focused work** — non-trivial
kernel/syscall engineering. Treat it as a known gap, not a bug to fix in
passing: when a task touches Linux sandboxing, exec rules, or capability
profiles, surface it as the separate pass it deserves rather than bolting it
onto unrelated changes.

When resolved, set `status: superseded` with a forward link.

See also [[map/core|core]], [[design/grant|grant]]. (Recorded 2026-05-30; the gap was
flagged earlier, ~2026-04-30.)
