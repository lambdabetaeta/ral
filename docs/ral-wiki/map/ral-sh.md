---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
covers_paths: [ral-sh/]
---

# Map: ral-sh

`ral-sh/src/main.rs` is a thin POSIX-bridge login shell dispatcher — the one
piece that stands *outside* the runtime ([[invariants/single-binary|single-binary]],
`docs/SPEC.md` §16.7). It carries no `ral-core` dependency.

It exists so `ral` can be registered as a login shell without breaking the
POSIX-assuming tools (`scp`, `rsync`, git-over-ssh, ansible) that a login shell
is expected to serve. `decide` classifies the invocation by parsed flags and tty
state, first match wins:

- `-c` present → `/bin/sh`. A `-c` invocation is a POSIX command string (`$SHELL
  -lc 'scp …'`); `ral`'s non-POSIX syntax must never see it, so this wins even
  alongside `-l`.
- `-l`/`-i`/`--login` present → `ral`. Editors and multiplexers launch a fully
  interactive session as `$SHELL -l` (VS Code) or `$SHELL -i` (tmux).
- no arguments and both stdin and stdout are ttys → `ral`. A bare interactive
  login.
- everything else (non-interactive, a script path, an opaque flag) → `/bin/sh`.

The login-shell `argv[0]`-prefixed-with-`-` convention is preserved across the
exec so the target sources its own profile. argv is read with `args_os`
throughout, forwarding non-UTF-8 arguments faithfully rather than panicking. On
Unix it refuses to run setuid as a safety check.

Both exec sites are **silent io-doors**: `exec_ral` and `exec_posix_sh` carry
`#[allow(clippy::disallowed_methods, reason = "[io-door:silent:respawn-…]")]`,
classifying the re-exec as infra plumbing — not a surfaced model exec image — so
the workspace door discipline accounts for them (see
[[map/exarch/io-surface|io-surface]]).

It is a registration shim, not a division of the runtime — see
[[invariants/single-binary|single-binary]] for why this does not violate the
one-executable rule.
