---
generated_at_commit: 451d1ab5
generated_at_date: 2026-09-22
covers_paths: [exarch/src/policy.rs, exarch/src/policy/]
---

# Map: exarch / policy

`policy/` composes the session's effective [[design/grant|grant]] —
[[map/core/capabilities|ral's capability lattice]] — from the CLI flags. The
boundary *is* ral's grant: exarch never invents its own sandbox, it just hands a
frozen `GrantStack` to the host that pushes it. There is no `Capabilities::meet`
— composition is layering, and the stack's own per-check fold is the
one meet that ever runs ([[decisions/260906_object-not-name|object-not-name]],
[[map/core/capabilities|capabilities]]).

```text
  ceiling = base ∨ extend_base?
  stack   = [ceiling, restrict₁, restrict₂, ..., deny(restricts)?, deny(credentials)?]
```

`for_invocation` builds that stack in a fixed order — **a single optional join
widens the ceiling into its own layer; every other attenuation is its own
pushed layer**:

- resolves the named base (`resolve_base`) as the ceiling;
- joins an optional `--extend-base` into the ceiling (widens it, still one
  layer);
- pushes each `--restrict` file as its own layer (attenuates);
- pushes a deny layer carving out each restrict file's own path (below);
- pushes a deny layer for
  [[decisions/260905_a-grant-does-not-hand-out-its-own-key|exarch's and synod's own credential files]]
  the same way, for every base but `dangerous`.

Composition emits no confused-deputy warning naming every prefix both
exec-admitted and writable. That predicate is not the one that matters —
in-projection overlap escalates nothing and every bake-in requires it, so such
a line would fire on every session naming `cwd:`, `/tmp`, and `tempdir:`, all
three deliberate ([[design/grant|grant]] §Concessions). The report lives
where it is asked for rather than announced: `ral`'s audit trail records
one `deputy` check per flagged prefix when a grant frame is entered
([[map/core/capabilities|capabilities]]).

Every profile is *frozen* as it loads — resolving each `~` / `xdg:` / `cwd:` /
`tempdir:` / `gitdir:` / `system:` sigil against the session's home, working
directory, and the platform's live tool roots inside
`ral_core::capability`'s decode pass — so composition runs entirely on
already-resolved `Capabilities` ([[design/capability-freeze|freeze boundary]]).
An `xdg:` path escaping `$HOME` is rejected at the profile that names it, before
composition could discard it. Loading reuses
`ral_core::capability::load_capabilities_from_*` — the same surface as ral's
`--capabilities <path>.ral` (`policy/load.rs` wraps it with exarch's error
format and the `absolute_in` cwd-join helper).

`base_layer(base_name, cwd)` resolves a bake-in base, frozen against the
child's working directory, as one way of naming the single layer a
[[design/agents|sub-agent]] spawn pushes. It does not take the parent: the
desk behind the [[map/exarch/builtins|`` exarch-agents `start `` tag]]
(`fleet/desk.rs`'s `fork_child`) clones the parent's own `GrantStack` and
pushes this layer onto the clone, so the same stack that carries the root's
authority also carries a spawned child's attenuation. The stack's per-check
fold ANDs every layer's verdict, so an added layer can only remove authority:
a spawn can reduce a child's reach but never escalate it past the parent's —
naming a base looser than the parent changes nothing (a network-off parent
stays offline even under `minimal`).

**A spawn's `grant` is one field with six spellings**, because at a spawn
`--base` and `--restrict` are the same act — the parent's stack is already
underneath, so a base pushed here can only narrow, exactly as a restrict does
([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]). The layer is
resolved by `SpawnGrant::layer` (`core/src/spawn_grant.rs`), which both seats
call: `` `inherit `` is ⊤, a layer the fold leaves no trace of;
`` `confined ``/`` `read-only ``/`` `edit-only ``/`` `reasonable `` reach
`base_layer` through the `GrantNarrower` the resolution is handed; and
`` `restrict R `` hands the record to
`ral_core`'s `decode_capability_map` — the same walker, off the same
`Form::Grant` declared table, that `grant [...] { body }` and each bake-in
profile below already pass through, so one keyset earns one wording.
`` `dangerous `` is a `--base` name only: at a spawn the lattice top is a layer
that says nothing, which *means* inherit-the-parent, and `` `inherit `` says
that without borrowing the root's reading of ⊤. A spawn's `R` gets no
self-denial layer either — it is a value computed in the parent's shell, with
no file on disk for the child to rewrite.

`deny_layer(paths, ctx)` is a pure constructor, not a mutator: it returns a
fresh `Capabilities` layer holding only those paths as `fs.deny_paths`, for the
caller to push, rather than folding them into whatever layer already carries
an `fs` opinion. `for_invocation` pushes two such layers, each making a kind
of bytes structurally unreachable: a restrict file's own path, so the agent
cannot edit the file that shapes its authority, and — once some layer already
holds an `fs` opinion — `provider::credential_files()`, so the agent cannot
read the credentials that authorise its own turn
([[decisions/260905_a-grant-does-not-hand-out-its-own-key|a-grant-does-not-hand-out-its-own-key]]).
Pushed as its own layer, a credential deny is one no profile can omit by
neglect and no `--extend-base` can widen back open — the stack's fold can
only narrow past it. **Only the user-supplied lexical form is pushed** — both
capability enforcers expand a deny entry to its canonical (and, on macOS,
firmlink) variants themselves, so canonicalising here would duplicate, less
completely, work that belongs to core. Each path is frozen through the same
lexer the grant decoder uses, so deny entries land as `NormalizedPrefix`es in
the grant-side normal form. The `--extend-base` file is *not* denied: it
widens the ceiling, so denying writes to it is a trust-source concern, not a
self-protection one. The credential deny is skipped for `dangerous` alone: it
attenuates nothing by contract, and pushing an `fs`-opinionated layer there
just to hold these denies would confine a session that asked not to be,
against an agent that can read the same bytes a hundred other ways.

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
`harness.rs::PERMISSION_LABELS` withholds it from `` exarch-agents `start ``,
as it does `dangerous` ([[design/agents|agents]]).

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
