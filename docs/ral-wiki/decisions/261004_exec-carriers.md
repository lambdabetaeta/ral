---
status: active
generated_at_commit: 6281520c
verified_at_commit: 6281520c
anchors: [carriers, trusted_real, head_shebang, launched_shebang, live_support, ExecAllow, allowed_files, emit_exec_rules, Warrant]
---

> **Amended 2026-10-06**
> ([[decisions/261006_one-table-two-instances|one-table-two-instances]]):
> `kernel()` enters each carrier into a copy of the table as an
> `ExecScope::Carrier` allow, ranked `Dir < Carrier < Exact < Name`, and asks
> the table's own `verdict` for each file once, so an authored exact deny or a
> veto beats a carrier by rank alone. A carrier holds a file by mutual
> containment, as a `File` rule does.

# The kernel exec set is one law on every platform

A confined command's descendants may `execve` exactly what

> the grant's rules ∪ their **carriers** ∪ the **platform base**

admit, on macOS and Linux alike. `ExecRules::kernel` emits the list, ordered
by `Rank` so that last-match-wins equals the in-process guard's
highest-rank-wins ([[decisions/261004_exec-rules|exec-rules]]); Seatbelt and
Landlock render it, and no renderer adds anything else. A grant therefore
means one thing on both kernels and in the guard
([[design/two-enforcers|two enforcers]]).

**The platform base is the loader and ral.** Linux needs `Execute` on the ELF
loader of every dynamic binary; macOS needs no `process-exec` on `dyld`
(measured) and keeps only `/Library/Apple/usr`, Rosetta's runtime. *Amended
2026-10-17*: Rosetta needs none either (measured, Darwin 25.5); the clause is
gone and the runtime is only read. ral's own
binary is the trampoline and every bundled tool.

**A carrier is what `execve` forwards to before the program's own code runs**:
a `#!` script's interpreter, up to four hops, and on macOS the system's
root-owned shims (`/bin/sh` to what `/private/var/select/sh` names, a
`/usr/bin` xcrun stub to its tool in the developer directory read from the
root-owned `xcode_select_link`, never `DEVELOPER_DIR`). Both kernels check a
script's interpreter (measured), so without carriers a script the guard admits
would die in the kernel. The parent computes them once, at spawn, over the
table's allowed files and the admitted program
(`core/src/sandbox/carriers.rs`); the guard never consults them; on Linux the
envelope binds each read-only.

**`env` is not followed.** `#!/usr/bin/env X` carries `env`; `X` needs its own
grant entry. Following it would read `PATH`, which the confined code controls.

**A carrier is admitted only on bytes the session's uid cannot author.**
`trusted_real` walks a path from `/` with `path::walk::walk_vetted`, the
handle-relative walk that splices links itself, vetting each object it passes:
every directory and the final file must be neither
owned nor writable by the euid, and every symlink not owned by it. The program
that names a hop and the hop itself must both pass. Otherwise
`#!/any/binary` written into an admitted script would admit any binary. A
user-writable symlink to a system binary fails too: its owner can repoint it.
Root owns everything, so it trusts nothing.

**The shebang grammar is the kernels', narrowed.** The `#!` line must end
within the first 256 bytes (Linux) or 512 (macOS); macOS also ends it at a
`#`. Leading blanks are skipped, the interpreter runs to the next blank, and
it must be absolute. Only the interpreter is kept. Any other shape carries
nothing, and the trampoline's `EACCES` hint names the interpreter to grant.

**Program behaviour is not forwarding.** `cc → cc1`, `clang → ld` and git's
helpers are execs a program chooses, which no header names. Toolchains are
grant data: the `system:` sigil carries each platform's tool and helper roots,
visible in the grant.

**Renderers.** Seatbelt emits one form per rule in order: an allow admits
`file-read* process-exec`, a deny or veto `process-exec` only — exec denies do
not deny reads, which are fs's. Landlock is allow-list only: it admits the
base, ral, every allowed file, and every allowed directory less what the
list, read back as a table, blocks beneath it; each file being emitted once at
its final verdict, that table judges as the guard does. Exec rules
carry `RealPath`s to the backends, and `render_real`, the one way from one to
a kernel spelling, adds only the firmlink twin and never reads the disk, so a
frozen grant cannot widen to a symlink's new target. Landlock judges inside
the bwrap envelope, whose names bwrap minted by following host symlinks, so
the parent opens each admit in the host with `RESOLVE_NO_SYMLINKS` and builds
the ruleset itself; the payload only enters it.

## Declared limits

- **The layer gates which paths may be `execve`d, never which code runs.** An
  allowed interpreter runs any script it can read. On Linux the admitted ELF
  loader runs any readable binary handed to it; macOS's `dyld` cannot be
  executed directly (measured)
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).
- **Landlock subtracts inside an allowed directory only over the tree at
  launch**: a program added afterwards to a directory holding a block is
  denied until the next launch, and a veto under a trusted write holds only at
  the in-process guard
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).
- **The kernel cannot see argv**, so `Only` renders as an allow.
- **Windows has no kernel exec layer.**

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** `trusted_real` walks from `/` through the one
walk with a per-object check (`walk_with`, the check given each `Vetted` step),
and returns a `Trace`; `walk_vetted` is gone. `ExecScope` is `ExecAllow`.

**Amended 2026-10-27.** `trusted_real` retains its `Located`. Each seed and
hop is judged by the projection's exclusion predicate on the walked trace,
then its `#!` is read through the system-vouched handle. Linkers meet the same
predicate. The parent carries its judged read of the launched interpreter in
`ral-warrant/4`, beside the interpreter exclusion; the child's refusal hint
opens nothing.
