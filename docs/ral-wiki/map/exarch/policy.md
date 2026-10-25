---
generated_at_commit: 773daa1062bb84e64c799cf502a0a5288c173f20
generated_at_date: 2026-10-21
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
  ceiling = base ◁ extension?
  stack   = [ceiling, restrict₁, restrict₂, ..., deny(restricts)?, deny(credentials)?, self]
```

`for_invocation` builds that stack in a fixed order — **a single optional extension
adds to the ceiling, once, in its own layer; every other attenuation is its own
pushed layer**:

- resolves the named base (`resolve_base`) as the ceiling;
- loads an optional `--extend` file as a `Fragment` and computes
  `base.extend(fragment)` as the ceiling (`◁`: an allowance never removes
  authority, an exclusion always wins, an axis the base leaves unrestricted
  stays so less the fragment's exclusions; still one layer;
  [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]);
- pushes each `--restrict` file as its own layer (attenuates);
- pushes a deny layer carving out each restrict file's own path (below);
- pushes a deny layer for
  [[invariants/a-grant-never-reaches-its-own-key|exarch's and synod's own credential files]]
  the same way, wherever some layer's fs restricts (the layer is pushed on the
  denotation, so `dangerous` extended with a positive-only fs gets none);
- pushes last the *self layer*: an exec exclusion of exarch's own binary, by
  the path it was pinned from, built as `deny_layer` is (`join_top` over a
  fragment of one exclusion), wherever some layer's exec restricts, so every
  spawned agent of a restricted session inherits it. The agent cannot start
  exarch as a process there; `dangerous` is ambient authority, its stack
  restricts nothing, and it needs no OS sandbox.

Composition emits no confused-deputy warning naming every prefix both
exec-admitted and writable. That predicate is not the one that matters —
in-projection overlap escalates nothing and every bake-in requires it, so such
a line would fire on every session naming `` `cwd ``, `` `absolute /tmp ``, and `` `tempdir ``, all
three deliberate ([[design/grant|grant]] §Concessions). The report lives
where it is asked for rather than announced: `ral`'s audit trail records
one `deputy` check per flagged prefix when a grant frame is entered
([[map/core/capabilities|capabilities]]).

Every profile is *resolved* as it loads — resolving each place (`` `home ``, `` `xdg-* ``, `` `cwd ``,
`` `tempdir ``, `` `scratch ``, `` `gitdir ``) and the `dir` selectors against the session's home, working
directory, scratch, and the host's live tool roots inside
`ral_core::authority::grant`'s construction pass (`authority/grant/decode.rs`,
`authority/grant/freeze.rs`) — authored resolution and host system-read discovery live
here, while `authority::policy` owns the resolved model and judgments, so
composition runs entirely on resolved `GrantLayer`s ([[design/capability-freeze|freeze boundary]]).
An `` `xdg-* `` place escaping `HOME` is rejected at the profile that names it, before
composition could discard it. Loading reuses
`ral_core::load::profile::load_capabilities_from_*` — the same surface as ral's
`--capabilities <path>.ral` (`policy/load.rs` wraps it with exarch's error
format and the `absolute_in` cwd-join helper).

`base_layer(base_name, cwd)` resolves a bake-in base, frozen against the
child's working directory, as one way of naming the single layer a
[[design/agents|sub-agent]] spawn pushes. It does not take the parent: the
desk behind the [[map/exarch/builtins|`` exarch-agents `start `` tag]]
(`agent/desk/agents.rs`'s `fork_seat`, through core's
`IdentityTransport::adopt_parked`) takes the parent's forked shell — which
carries the parent's own `GrantStack` — and pushes this layer onto it
(`SpawnGrant::narrow_onto`), so the same stack that carries the root's
authority also carries a spawned child's attenuation. The stack's per-check
fold ANDs every layer's evidence, so an added layer can only remove authority:
a spawn can reduce a child's reach but never escalate it past the parent's —
naming a base looser than the parent changes nothing (a network-off parent
stays offline even under `minimal`).

**A spawn's `grant` is one field with six spellings**, because at a spawn
`--base` and `--restrict` are the same act — the parent's stack is already
underneath, so a base pushed here can only narrow, exactly as a restrict does
([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]). The layer is
resolved by `SpawnGrant::layer` (`core/src/authority/grant/spawn.rs`), which both seats
call: `` `inherit `` is ⊤, a layer the fold leaves no trace of;
`` `confined ``/`` `read-only ``/`` `edit-only ``/`` `reasonable `` reach
`base_layer` through the `GrantNarrower` the resolution is handed; and
`` `restrict R `` hands the record to
`ral_core::authority::grant::decode_capability_map` — the same walker, off the same
`Form::Grant` declared table, that `grant [...] { body }` and each bake-in
profile below already pass through, so one keyset earns one wording.
`` `dangerous `` is a `--base` name only: at a spawn the lattice top is a layer
that says nothing, which *means* inherit-the-parent, and `` `inherit `` says
that without borrowing the root's reading of ⊤. A spawn's `R` gets no
self-denial layer either — it is a value computed in the parent's shell, with
no file on disk for the child to rewrite.

`deny_layer(paths, ctx)` is a pure constructor, not a mutator: it returns a
fresh `GrantLayer` holding only those paths as its fs exclusions, for the
caller to push, rather than folding them into whatever layer already carries
an `fs` opinion. Each path is frozen by `freeze::host_absolute`, and one it
refuses — not UTF-8, or not absolute under this host's rules — fails the
layer whole, since a deny that named nothing would fail open.
`for_invocation` pushes two such layers, each making a kind
of bytes structurally unreachable: a restrict file's own path, so the agent
cannot edit the file that shapes its authority, and — once some layer already
holds an `fs` opinion — `provider::credential_files()`, so the agent cannot
read the credentials that authorise its own turn
([[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]]).
Pushed as its own layer, a credential deny is one no profile can omit by
neglect and no `--extend` can widen back open — the stack's fold can
only narrow past it. **Only the user-supplied lexical form is pushed** — both
capability enforcers expand a deny entry to its canonical (and, on macOS,
firmlink) variants themselves, so canonicalising here would duplicate, less
completely, work that belongs to core. Each path is frozen through the same
lexer the grant decoder uses, so deny entries land as `FrozenPath`es in
the grant-side normal form. The `--extend` file is *not* denied: it
extends the ceiling, so denying writes to it is a trust-source concern, not a
self-protection one. The credential deny is pushed only where some layer's fs restricts: for plain
`dangerous` it would confine a session that asked not to be, against an agent
that can read the same bytes a hundred other ways.

`policy/base.rs` embeds the six bake-in profiles from `exarch/data/*.exarch.ral`
via `include_str!`, ordered from most to least authority:

- `dangerous` — the lattice top, no attenuation;
- `reasonable` — default; everyday tooling + standard binary dirs;
- `edit-only` — reasonable's reads, writes to the working tree and scratch,
  network on; for patch/refactor jobs that never run a build;
- `read-only` — reasonable's reads/exec, writes only to scratch;
  the scratch and its tool homes are shared tree-wide, so a narrowed child can plant files a wider sibling runs (accepted; see [[decisions/261013_the-seed-rides-the-protocol-sealed|the-seed-rides-the-protocol-sealed]]);
- `minimal` — system binaries + cwd + scratch + net + chdir; a deliberately
  narrow base for additive `--extend`;
- `confined` — offline build jail; host binaries by subpath, with explicit
  bare-name entries for core's bundled tools (which have no path to match).

**Every profile that restricts exec must name the bundled tools** (`core/src/uutils.rs`
— coreutils, and diffutils/ripgrep under their features). `Head::resolve` makes
those a `Program::Tool` and skips the `PATH` walk entirely, so they reach the
in-process guard as a tool with no place on disk: a directory prefix cannot
match one, and silence denies it. That is why `read-only`, `edit-only`,
`reasonable` and `confined` each carry a per-name coreutils block that looks
redundant beside their `` `dir `system `` rule and is not.
`every_grantable_base_settles_the_bundled_tools` pins it, per build
configuration — turn on the `ripgrep` feature without naming `rg` in a profile
and it fails, naming the tool.

The consequence for spawning: a base whose `exec` is prefixes alone is
unusable as a child's `grant`, since the child cannot widen its own ceiling to
recover `ls`. `minimal` is such a base, and is offered by `--base` only —
`policy::SPAWN_BASES` withholds it from `` exarch-agents `start ``,
as it does `dangerous` ([[design/agents|agents]]).

Each is a ral script whose terminal expression is a map shaped like the argument
of `grant [...] { body }`, loaded through
`ral_core::load::profile::load_capabilities_from_str` — the same surface
`--capabilities <path>.ral` consumes at the ral CLI. Two surfaces, one model. The
host reads the resulting authority only through core's accessors, never its
representation ([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]).

**The bake-ins name exec authority portably through the `dir` selectors** —
[[map/core/capabilities|core]]'s spelling of live tool roots, each a row of one
table (`ral_core::authority::grant::toolchains`, rules in [[design/grant|grant]]
§Selectors). `` `dir `system `` is the standard binary dirs on Unix (Homebrew is
its own selector, `` `dir `homebrew ``, expanding to the prefixes that exist,
`HOMEBREW_PREFIX` first); `%SystemRoot%\System32`, the PowerShell home, and
Git-for-Windows' `usr\bin` on Windows. `reasonable` lists a selector for every
package manager in the table and writes no read entry for what one runs;
running reads cover it. It excludes `~/.dotnet/corefx` from `fs`. No profile
writes `xdg-cache`: `reasonable` and `minimal` write the working tree and scratch only,
and exarch points `XDG_CACHE_HOME` and the legacy tool homes into the scratch
([[map/exarch|exarch]]). `RUSTUP_HOME` is not redirected, since it holds what
runs. Two consequences ride the selectors:

- interactive shells are denied by literal override in every attenuated
  profile — `bash`/`zsh` and their Windows analogues `cmd`/`powershell`/`pwsh`
  (which `` `dir `system `` would otherwise admit via `System32`); `sh` stays allowed
  for `configure`/`make` shell-outs;
- `minimal` leaves Homebrew unlisted, so brew tools are opt-in through
  `--extend`; `reasonable`, `confined`, `edit-only` and `read-only` add
  `` `dir `homebrew ``.

The decoder drops *dead grants* as a profile loads: a bare exec name that finds
neither a file on the decoding `PATH` nor a tool this platform bundles (the
Unix-only coreutils off Unix) makes no allowance and keeps its veto
(`name_rules`), and a path rooted under a foreign platform's convention, a Unix
literal on Windows, is discarded — a rendered profile never advertises authority
the platform cannot back.

The in-module tests pin the load-bearing per-profile properties so a future edit
can't silently widen a jail:

- `dangerous` is the lattice top;
- `git` admitted in `reasonable`/`read-only` (commit flows work without
  `--extend`); `minimal` admits only the *system* git under `/usr/bin/`
  (a brew git is opt-in via the git extension);
- every exec-declaring base carries an `Allow` for each live system tool root;
- `read-only` reads but does not write `` `cwd ``;
- `confined` is net-off, admits host execs by subpath and only bundled tools by
  bare name, and has no home-reaching prefixes;
- `` `cwd ``/`` `tempdir ``/`` `scratch `` places freeze into the per-invocation working,
  temp and scratch dirs without exarch injecting them dynamically: the scratch is
  made before the policy freezes, and `FreezeCtx::with_scratch` carries its spelling.

`exarch/examples/git.exarch.ral` is the canonical `--extend` that lifts
`minimal`/`confined` into a git-capable shape; a test pins that extending `minimal` with it keeps `git` admitted and adds
`~/.gitconfig`, and that `FsPolicy::join`
**unions** deny sets — a one-sided veto survives the join, so an overlay stays
purely additive without re-stating the base's credential denies.

The system-prompt assembly that renders this frozen authority into the `Grant`
section lives on [[map/exarch|exarch]], which owns `prompt.rs` and `data/`.
