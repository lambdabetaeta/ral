---
verified_at_commit: d9abfb52
verified_at_date: 2026-09-11
against: [design/two-enforcers, design/syscalls-are-effects, decisions/260715_vm-workspaces-cross-by-copy]
---

# gVisor

Google's `runsc`, Apache-2.0 (`.scratch/gvisor` in this checkout, not vendored,
not linked). **gVisor is a userspace reimplementation of the Linux syscall
ABI, in Go: the Sentry intercepts every syscall a container makes and answers
most of them itself**, with a second process — the Gofer — proxying the
filesystem calls it does not serve directly. Both run under their own fixed
seccomp allow-list, since each is one program whose syscall surface is known
in advance.

**ral is not that.** ral is authority over effects composed from OS
primitives — bwrap, Landlock, seccomp — under two enforcers, not a
reimplementation of the kernel interface a confined program sees
([[design/two-enforcers|two-enforcers]], [[design/syscalls-are-effects|syscalls-are-effects]]).
A confined command still talks to the real kernel; ral narrows what it may
ask for rather than answering on the kernel's behalf. Where ral asks "should
this envelope run hostile code at all", the answer is
[[decisions/260715_vm-workspaces-cross-by-copy|the VM boundary]]: the Sentry
is gVisor's answer to the question ral assigns to a VM, not to bwrap.

## What this change takes from it

- **Typed syscall rules with a printer.** `runsc`'s filter config
  (`runsc/boot/filter`, `runsc/fsgofer/filter`) is Go structs naming a
  syscall and its allowed argument shapes, compiled to BPF at startup — the
  same shape [[decisions/260906_seccomp-is-a-typed-deny-set|our own deny-set]]
  now takes, one level removed: theirs is the only program: an allow-list
  each of these two processes owns outright; ours is a deny-set stacked over
  whatever `exec` admits.
- **Weakenings reported as first-class output.** `config.Warnings` computes
  what a chosen configuration gives up and returns it as data, not a log
  line, for the caller to print. `HostEnvelope` is ral's own version: a
  probed value naming which invariant this host cannot hold, printed by
  `RAL_DUMP_SANDBOX_PROFILE` rather than discovered by a confined command
  failing silently.
- **Confining the confiner.** The Gofer runs under its own, tighter
  seccomp-BPF program — a process trusted with real filesystem access still
  gets the narrowest surface its own job needs, not the Sentry's. ral's
  bundled-tool child is the analogous case: a future tighter filter keyed on
  *who* the payload is, once `Filter` has shown the deny-set composes. Not
  in this change ([[decisions/260906_seccomp-is-a-typed-deny-set|the ADR's]]
  "Not in this change").

## What it does not

- **The Sentry itself.** Reimplementing the syscall ABI in userspace is a
  different security boundary than ral's — closer to the VM workspace's job
  ([[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]])
  than to an envelope around one external command.
- **OCI plumbing.** `runsc`'s config surface is a container runtime's; ral
  has no image, no bundle, no `runc` shim to be compatible with.

One loose end noted in passing: `sandboxexec/proto` declares a
`domain_allowlist` field (`sandbox_options.proto`) for network-domain
filtering — this checkout of gVisor never reads it back out. Not a lead for
ral; `net` stays a bit, not a list (decision 2 of the deny-set ADR).
