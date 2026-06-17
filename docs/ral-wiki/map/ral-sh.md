---
generated_at_commit: c164cff
generated_at_date: 2026-05-31
covers_paths: [ral-sh/]
---

# Map: ral-sh

`ral-sh/src/main.rs` is a thin POSIX-bridge login shell dispatcher — the one
piece that stands *outside* the runtime ([[invariants/single-binary|single-binary]],
`docs/SPEC.md` §21.1). It carries no `ral-core` dependency.

It exists so `ral` can be registered as a login shell without breaking the
POSIX-assuming tools (`scp`, `rsync`, git-over-ssh, ansible) that a login shell
is expected to serve. Dispatch is two cases:

- an interactive invocation (stdin *and* stdout both ttys, no arguments) execs
  `ral`;
- everything else (non-interactive, `-c`, a script path) forwards to `/bin/sh`.

The login-shell `argv[0]`-prefixed-with-`-` convention is preserved across the
exec so the target sources its own profile. On Unix it refuses to run setuid as a
safety check.

It is a registration shim, not a division of the runtime — see
[[invariants/single-binary|single-binary]] for why this does not violate the
one-executable rule.
