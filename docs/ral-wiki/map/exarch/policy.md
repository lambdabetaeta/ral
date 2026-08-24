---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
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
- adds each restrict file's path to `fs.deny_paths` (below);
- lints the composed ceiling for confused-deputy prefixes
  (`lint_deputy_prefixes` over `ral_core::capability::deputy_prefixes`) — a
  warning, never a denial, judged after every join and meet has run. It fires
  on every bake-in: `reasonable` alone flags `cwd:`, `/tmp`, and `tempdir:`,
  all three deliberate, since in-projection overlap escalates nothing
  ([[design/grant|grant]] §Concessions). The predicate is thus not the one that
  matters, and the lint's standing is open.

Every profile is *frozen* as it loads — resolving each `~` / `xdg:` / `cwd:` /
`tempdir:` / `gitdir:` / `system:` sigil against the session's home, working
directory, and the platform's live tool roots inside
`ral_core::capability`'s decode pass — so composition runs entirely on
already-resolved `Capabilities` ([[design/capability-freeze|freeze boundary]]).
An `xdg:` path escaping `$HOME` is rejected at the profile that names it, before
composition could discard it. Loading reuses
`ral_core::capability::load_capabilities_from_*` — the same surface as ral's
`--capabilities <path>.ral` (`policy/load.rs` wraps it with exarch's error
format, the `absolute_in` cwd-join helper, and the deputy lint).

`narrow(parent, base_name, cwd)` is the **meet-sibling of `for_invocation`**: it
freezes the named base against the child's working directory and returns
`parent ⊓ base`. Where `for_invocation` builds the *root's* ceiling (a join then
meets), `narrow` bounds a *child's* — so the same lattice that grants the root's
authority also attenuates a spawned [[design/agents|sub-agent]]'s. Meet only ever
removes authority and the result is ≤ both operands, so a spawn can reduce a
child's reach but never escalate it past the parent's: naming a base looser than
the parent changes nothing (a network-off parent stays offline even under
`minimal`, since `false ⊓ true = false`), and `dangerous` — the lattice top —
leaves the parent's authority verbatim. The desk behind the
[[map/exarch/builtins|`` agents `start `` tag]] (`fleet/desk.rs`) calls it at
the spawn site with the spawn record's mandatory `grant` base.

`deny_paths` makes a restrict file's own bytes structurally unreachable: a
restrict file shapes the agent's authority, so the agent must not be able to edit
it. **Only the user-supplied lexical form is pushed** — both capability enforcers
expand a deny entry to its canonical (and, on macOS, firmlink) variants
themselves, so canonicalising here would duplicate, less completely, work that
belongs to core. Each path is frozen through the same lexer the grant decoder
uses, so deny entries land as `NormalizedPrefix`es in the grant-side normal form.
The `--extend-base` file is *not* denied: it widens authority, so denying writes
to it is a trust-source concern, not a self-protection one.

`policy/base.rs` embeds the six bake-in profiles from `exarch/data/*.exarch.ral`
via `include_str!`, ordered from most to least authority:

- `dangerous` — `Capabilities::root`, lattice top, no attenuation;
- `reasonable` — default; everyday tooling + standard binary dirs;
- `edit-only` — reasonable's reads, writes to the working tree and scratch,
  network on; for patch/refactor jobs that never run a build;
- `read-only` — reasonable's reads/exec, writes only to scratch;
- `minimal` — system binaries + cwd + scratch + net + chdir; a deliberately
  narrow base for additive `--extend-base`;
- `confined` — offline build jail; host binaries by subpath, with explicit
  bare-name entries for core's bundled tools (which have no path to match).

**Every profile that restricts exec must name the bundled tools** (`core/src/uutils.rs`
— coreutils, and diffutils/ripgrep under their features). `command::vet` routes
those to an `ExecImage::BundledTool` and skips the `PATH` probe entirely, so they
reach the gate as a bare name with no resolved path: a directory prefix cannot
match one, and silence denies it. That is why `read-only`, `edit-only`,
`reasonable` and `confined` each carry a per-name coreutils block that looks
redundant beside their `system:` subpath rule and is not.
`every_grantable_base_settles_the_bundled_tools` pins it, per build
configuration — turn on the `ripgrep` feature without naming `rg` in a profile
and it fails, naming the tool.

The consequence for spawning: a base whose `exec` is prefixes alone is
unusable as a child's `grant`, since the child cannot widen its own ceiling to
recover `ls`. `minimal` is such a base, and is offered by `--base` only —
`harness.rs::PERMISSION_LABELS` withholds it from `` agents `start ``
([[design/agents|agents]]).

Each is a ral script whose terminal expression is a map shaped like the argument
of `grant [...] { body }`, loaded through
`ral_core::capability::load_capabilities_from_str` — the same surface
`--capabilities <path>.ral` consumes at the ral CLI. Two surfaces, one model. The
host reads the resulting authority only through core's accessors, never its
representation ([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]).

**The bake-ins name exec authority portably through the `system:` sigil** —
[[map/core/capabilities|core]]'s spelling of the platform's live tool roots
(`ral_core::path::sigil::system_tool_roots`): the standard binary dirs plus a
Homebrew tree when the host has one on Unix; `%SystemRoot%\System32`, the
PowerShell home, and Git-for-Windows' `usr\bin` on Windows. Two consequences
ride it:

- interactive shells are denied by literal override in every attenuated
  profile — `bash`/`zsh` and their Windows analogues `cmd`/`powershell`/`pwsh`
  (which `system:` would otherwise admit via `System32`); `sh` stays allowed
  for `configure`/`make` shell-outs;
- `minimal` carves the Homebrew tree back out with an explicit
  `/opt/homebrew/` deny, so `system:` folding it in when present never widens
  minimal's "system tools only" narrowing — brew tools stay opt-in.

`resolve_base` also drops *dead grants* as a profile loads: exec literals
naming the Unix-only bundled coreutils (`drop_dead_exec_grants`) are removed
off Unix, and the freeze pass discards foreign-rooted Unix literals on
Windows — a rendered profile never advertises authority the platform cannot
back.

The in-module tests pin the load-bearing per-profile properties so a future edit
can't silently widen a jail:

- `dangerous` is `Capabilities::default` (lattice top);
- `git` admitted in `reasonable`/`read-only` (commit flows work without
  `--extend-base`); `minimal` admits only the *system* git under `/usr/bin/`
  (its Homebrew deny keeps a brew git opt-in via the git extension);
- every exec-declaring base carries an `Allow` for each live system tool root
  (`minimal`'s Homebrew deny the one documented exception);
- `read-only` reads but does not write `cwd:`;
- `confined` is net-off, admits host execs by subpath and only bundled tools by
  bare name, and has no home-reaching prefixes;
- `cwd:`/`tempdir:` sigils freeze into the per-invocation working and temp dirs
  without exarch injecting them dynamically.

`exarch/examples/git.exarch.ral` is the canonical `--extend-base` that lifts
`minimal`/`confined` into a git-capable shape; a test pins that joining it into
`minimal` keeps `git` admitted and adds `~/.gitconfig`, and that `FsPolicy::join`
**unions** deny sets — a one-sided veto survives the join, so an overlay stays
purely additive without re-stating the base's credential denies.

The system-prompt assembly that renders this frozen authority into the `Grant`
section lives on [[map/exarch|exarch]], which owns `prompt.rs` and `data/`.
