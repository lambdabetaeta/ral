---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
covers_paths: [exarch/src/policy.rs, exarch/src/policy/]
---

# Map: exarch / policy

`policy/` composes the session's effective [[design/grant|grant]] —
[[map/core/capabilities|ral's capability lattice]] — from the CLI flags. The
boundary *is* ral's grant: exarch never invents its own sandbox, it just hands a
frozen `Capabilities` to the host that pushes it.

```text
  ceiling   = base ∨ extend_base?
  effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
```

`for_invocation` composes the lattice in a fixed order — **a single optional join
widens the ceiling, then any number of commuting meets attenuate from it**:

- resolves the named base (`resolve_base`);
- joins an optional `--extend-base` (widens the ceiling);
- meets each `--restrict` file (attenuates);
- adds each restrict file's path to `fs.deny_paths` (below).

Every profile is *frozen* as it loads — resolving each `~` / `xdg:` / `cwd:` /
`tempdir:` sigil against the session's home and working directory inside
`ral_core::capability`'s decode pass — so composition runs entirely on
already-resolved `Capabilities` ([[design/capability-freeze|freeze boundary]]).
An `xdg:` path escaping `$HOME` is rejected at the profile that names it, before
composition could discard it. Loading reuses
`ral_core::capability::load_capabilities_from_*` — the same surface as ral's
`--capabilities <path>.ral` (`policy/load.rs` only wraps it with exarch's error
format and the `absolute_in` cwd-join helper).

`deny_paths` makes a restrict file's own bytes structurally unreachable: a
restrict file shapes the agent's authority, so the agent must not be able to edit
it. **Only the user-supplied lexical form is pushed** — both capability enforcers
expand a deny entry to its canonical (and, on macOS, firmlink) variants
themselves, so canonicalising here would duplicate, less completely, work that
belongs to core. Each path is frozen through the same lexer the grant decoder
uses, so deny entries land as `NormalizedPrefix`es in the grant-side normal form.
The `--extend-base` file is *not* denied: it widens authority, so denying writes
to it is a trust-source concern, not a self-protection one.

`policy/base.rs` embeds the five bake-in profiles from `exarch/data/*.exarch.ral`
via `include_str!`, ordered from most to least authority:

- `dangerous` — `Capabilities::root`, lattice top, no attenuation;
- `reasonable` — default; everyday tooling + standard binary dirs;
- `read-only` — reasonable's reads/exec, writes only to scratch;
- `minimal` — system binaries + cwd + scratch + net + chdir; a deliberately
  narrow base for additive `--extend-base`;
- `confined` — offline build jail, exec by subpath only.

Each is a ral script whose terminal expression is a map shaped like the argument
of `grant [...] { body }`, loaded through
`ral_core::capability::load_capabilities_from_str` — the same surface
`--capabilities <path>.ral` consumes at the ral CLI. Two surfaces, one model. The
host reads the resulting authority only through core's accessors, never its
representation ([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]).

The in-module tests pin the load-bearing per-profile properties so a future edit
can't silently widen a jail:

- `dangerous` is `Capabilities::default` (lattice top);
- `git` admitted in `reasonable`/`read-only` (commit flows work without
  `--extend-base`), denied in `minimal` (keeps it a deliberate-opt-in base);
- `bash`/`zsh` denied despite `/bin/` sitting in exec dirs — a literal `'deny'`
  overrides the subpath admit, while `/bin/sh` stays allowed as build
  infrastructure;
- `read-only` reads but does not write `cwd:`;
- `confined` is net-off, exec-by-subpath-only (no bare-name admits), no
  home-reaching prefixes;
- `cwd:`/`tempdir:` sigils freeze into the per-invocation working and temp dirs
  without exarch injecting them dynamically.

`exarch/examples/git.exarch.ral` is the canonical `--extend-base` that lifts
`minimal`/`confined` into a git-capable shape; a test pins that joining it into
`minimal` admits `git` and adds `~/.gitconfig`, and that `FsPolicy::join`
intersects deny sets (a one-sided deny means the other side admits, so the
widened result must too).

The system-prompt assembly that renders this frozen authority into the `Grant`
section lives on [[map/exarch|exarch]], which owns `prompt.rs` and `data/`.
