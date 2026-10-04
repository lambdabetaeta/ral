---
status: active
generated_at_commit: f4e88bce
verified_at_commit: f4e88bce
anchors: [carriers, trusted_real, shebang, kernel, Rank, ExecRule, emit_exec_rules, Sbpl]
---

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
(measured) and keeps only `/Library/Apple/usr`, Rosetta's runtime. ral's own
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
`trusted_real` walks a path from `/` one component at a time with `lstat`,
following links itself: every directory and the final file must be neither
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
visible in the grant and its dump.

**Renderers.** Seatbelt emits one form per rule in order: an allow admits
`file-read* process-exec`, a deny or veto `process-exec` only — exec denies do
not deny reads, which are fs's. Landlock is allow-list only: it admits the
base, ral and every allowing rule, and renders no deny.

## Declared limits

- **The layer gates which paths may be `execve`d, never which code runs.** An
  allowed interpreter runs any script it can read. On Linux the admitted ELF
  loader runs any readable binary handed to it; macOS's `dyld` cannot be
  executed directly (measured)
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).
- **Landlock cannot subtract inside an allowed directory**, so on Linux a deny
  or veto under an allowed directory holds only at the in-process guard.
- **The kernel cannot see argv**, so `Only` renders as an allow.
- **Windows has no kernel exec layer.**
