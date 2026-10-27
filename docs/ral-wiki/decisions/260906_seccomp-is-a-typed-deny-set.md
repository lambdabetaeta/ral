---
status: active
generated_at_commit: 781d88844423a2af09f5e9a87e0d902fc4f15cff
---

# The seccomp filter is a typed deny-set

Prompted by reading gVisor (`.scratch/gvisor`, Apache-2.0; see
[[related/gvisor|gvisor]]) against `core/src/sandbox/`. Nothing of gVisor's is
copied; what it shows is that our one un-typed enforcer was the seccomp
program: eight `libc::SYS_*` numbers hand-packed into `sock_filter` bytes
(`linux.rs::build_seccomp_filter`, `BpfProg`, `apply_seccomp`). Every other
layer of the envelope — the bwrap argv, the Landlock `Layer`, `HostEnvelope`
— is a value, tested from literals. Replaced by `sandbox/linux/seccomp.rs`: a
value, `seccomp::Filter`, that renders to BPF for bwrap and *names* a denied
syscall for `diag` — the same data enforces and explains, so the two cannot
drift.

That un-typed state had a user-visible cost: `diag/linux.rs` read a
`type=1326` record, extracted `syscall=101`, and `diag.rs::build_hint`'s
no-path branch told the user *"widen the grant's fs read set"*. `101` is
`ptrace` on x86-64. No grant is involved; the advice was wrong and the number
unnamed.

## Decisions

1. **Deny-set, not allow-list.** gVisor's Sentry runs under a syscall
   allow-list because it is one fixed program. An envelope runs whatever
   `exec` admits — compilers, JVMs, `podman` — and an allow-list turns every
   unanticipated syscall into a SIGSYS the wiki already calls "opaque, after
   the fact and unattributable" ([[design/two-enforcers|two-enforcers]]). The
   deny-set stays; it grows, and gains argument conditions.

2. **Projection-independent.** The filter is an envelope invariant ("rows
   below the rule" in [[design/two-enforcers|two-enforcers]]), keyed on
   nothing in the grant, so it is built once per process, not per launch. A
   `net: false`-dependent `socket` rule was considered and rejected:
   `--unshare-net` already holds `net`, and a second copy of a promise is a
   place for two enforcers to disagree.

3. **No mounting, no containers inside a grant.** The mount family is
   killed; user-namespace creation and `setns` are refused with `EPERM`.
   [[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]
   already says mount authority in the payload undoes every deny mask and
   that a nested envelope must never happen; this is the kernel-side
   statement of both. Rootless `podman`/`docker`, Chrome's sandbox and a
   nested `bwrap` are therefore refused under any grant — with an errno, so
   they print their own error, and `diag` says the refusal was the
   envelope's, not the grant's.

4. **One value, two readers.** `seccomp::Filter` renders to BPF for
   bwrap and names a denied syscall number for `diag`.

5. **`seccompiler` compiles the BPF.** rust-vmm's crate, 0.5.0 (Apache-2.0 OR
   BSD-3), x86_64 and aarch64, arch-validation prologue built in, argument
   conditions, `KillThread`/`Errno` actions. It deletes `BpfProg` and the
   hand-written jump offsets. The four concepts it adds — filter, rule,
   condition, action — are the four we would otherwise write.

6. **Two verdicts, several programs.** A `seccompiler` filter has *one*
   `match_action`, so `Kill` and each distinct `Errno` compile to their own
   BPF program — three here (`Kill`; `Errno(EPERM)` for `unshare`/`clone`,
   `setns`, `ioctl`, socket families and io_uring; `Errno(ENOSYS)` for `clone3`). bwrap stacks them, one
   `--add-seccomp-fd` each (it refuses that flag beside `--seccomp`) — the
   kernel applies every installed filter and takes the most severe result,
   so program order is immaterial. `Kill` stays `SECCOMP_RET_KILL_THREAD`,
   as before.

## The deny-set

Criterion for `Kill`: *no unprivileged program has a legitimate use for it
inside a confined command, and its kernel surface is historically
exploitable.* Criterion for `Errno`: *a legitimate tool may attempt it, and
should be told no in its own words.* Each rule carries a one-line `why` that
`diag` repeats verbatim.

| syscalls | condition | verdict | why |
|---|---|---|---|
| `ptrace`, `process_vm_readv`, `process_vm_writev` | — | Kill | reads or rewrites another process; the envelope hides the host's table, this closes the same-uid pair inside it |
| `perf_event_open`, `bpf` | — | Kill | loads programs into the kernel; classic privilege-escalation surface |
| `kexec_load`, `kexec_file_load`, `reboot`, `swapon`, `swapoff` | — | Kill | machine state |
| `init_module`, `finit_module`, `delete_module` | — | Kill | kernel code |
| `keyctl`, `add_key`, `request_key` | — | Kill | the kernel keyring: cross-process state the envelope does not model |
| `userfaultfd` | — | Kill | the race-widening primitive in most modern exploit chains |
| `open_by_handle_at` | — | Kill | opens by inode handle, bypassing every path the envelope bound |
| `mount`, `umount2`, `pivot_root`, `move_mount`, `open_tree`, `fsopen`, `fsmount`, `fsconfig`, `fspick`, `mount_setattr` | — | Kill | mount authority inside the envelope hides or reshapes the deny masks |
| `unshare`, `clone` | arg0 & `CLONE_NEWUSER` ≠ 0 | Errno(EPERM) | a user namespace is root over a fresh mount tree — a container inside the grant, which the envelope forbids |
| `setns` | — | Errno(EPERM) | joining another namespace is leaving this one |
| `clone3` | — | Errno(ENOSYS) | its flags live in a struct seccomp cannot read; ENOSYS makes glibc and Rust std fall back to `clone`, which the row above inspects (Docker's default profile does the same) |
| `ioctl` | `arg1 in {TIOCSTI, TIOCLINUX}` | Errno(EPERM) | terminal input injection (CVE-2017-5226); `--new-session` closes it, this is the seccomp half the namespace ADR deferred to the terminal trampoline, independent of it |
| `socket` | family outside `{AF_UNIX, AF_INET, AF_INET6, AF_NETLINK}`, or `AF_NETLINK` with protocol other than `NETLINK_ROUTE` | Errno(EPERM) | a family no grant models: VSOCK reaches a hypervisor's host, netlink beyond routing reaches audit, device events and every socket's diagnostics, and the rest is hardware or kernel surface |
| `socketpair` | family other than `AF_UNIX` | Errno(EPERM) | a pair outside the Unix family is that family's kernel surface |
| `io_uring_setup`, `io_uring_enter`, `io_uring_register` | always | Errno(EPERM) | creates and connects sockets with no syscall this filter sees |

Left out deliberately: `chroot` (harmless without mount).

## Amendment: socket families and io_uring

**A socket names a peer; the deny-set refuses families whose peers no grant
models, independently of `net`.** Only Unix, IPv4, IPv6 and routing netlink
sockets remain admitted. An unprivileged `NETLINK_ROUTE` socket is read-only
and cannot address a userspace peer; `getifaddrs` and `getaddrinfo` with
`AI_ADDRCONFIG` need it. Listing the admitted families also refuses future
families and their module autoloading. Unix socket reach remains governed
elsewhere, as described in [[design/two-enforcers|two-enforcers]].

The earlier objection to refusing io_uring no longer holds: the verdict is
`EPERM`. `IORING_OP_SOCKET` (Linux 5.19) and `IORING_OP_CONNECT` (5.5) create
and connect sockets without a `socket(2)` syscall for the filter to inspect.
`kernel.io_uring_disabled=2` already answers `EPERM`, and libuv, MariaDB and
QEMU fall back on it.

The costs are explicit:

- `NETLINK_KOBJECT_UEVENT`: libusb fails to initialise, so `lsusb` and
  libusb's users refuse to start; libudev monitors likewise.
- `NETLINK_AUDIT`: `sudo` and PAM tools warn and continue.
- `NETLINK_SOCK_DIAG`: `ss` falls back to `/proc/net`.
- io_uring: `tokio-uring`, `glommio` and `monoio` binaries fail at startup,
  as on a host with `kernel.io_uring_disabled=2`.
- `AF_ALG`: cryptsetup's kernel backend and OpenSSL's opt-in afalg engine
  cannot open their kernel crypto sockets.

Each syscall has one rule and one reason. Its condition is a disjunction of
non-empty conjunctions; `ArgNotIn` compares the argument's low 32 bits against
every kept value with `Ne`. A const check rejects duplicate syscalls and
empty alternatives, conjunctions or kept-value lists. `Programs` retains the
compiled BPF, including the x32 guard, and installs it without allocating or
locking; bwrap's byte parcels are serialised per launch. An errno refusal
leaves an audit record only where auditing is enabled for that process;
`diag` attributes any record it finds through the same `explain` data.

## The x32 guard

`seccompiler`'s own arch-validation prologue
(`backend/bpf.rs::build_arch_validation_sequence`) only compares
`seccomp_data.arch` against the running ABI's `AUDIT_ARCH`; it never reads
bit 30 of `nr`. x32 shares `AUDIT_ARCH_X86_64`, so an x32 syscall passes that
prologue, matches no key in our syscall-numbered maps, and reaches the kill
program's own mismatch action — `Allow`. The guard the deleted
`build_seccomp_filter` hand-wrote therefore stays ours: a fourth stacked
program, x86-64 only, a complete filter in its own right — the kernel
evaluates every stacked program independently, so a guard riding on another
program's arch check would never see the foreign-ABI syscalls at all.

## Rejected

- **An allow-list**: gVisor's own answer, but it is one fixed Sentry program;
  an envelope runs whatever `exec` admits, and an allow-list would turn every
  unanticipated syscall into an unattributable SIGSYS.
- **A projection-keyed `socket` rule** under `net: false` (decision 2):
  `--unshare-net` already holds `net`, and a second copy of a promise is a
  place for two enforcers to disagree.
- **Folding `ENOSYS` into `EPERM`** to save a program: the fallback in
  glibc/Rust std from `clone3` to `clone` is keyed on `ENOSYS` specifically.

See also [[design/two-enforcers|two-enforcers]],
[[decisions/260906_landlock-exec-layer|landlock-exec-layer]],
[[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]],
[[related/gvisor|gvisor]]. `docs/SPEC.md` §12.11.
