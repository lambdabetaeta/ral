---
generated_at_commit: 3fcae877
generated_at_date: 2026-11-13
covers_paths: [exarch/src/policy.rs, exarch/src/policy/]
---

# Map: exarch / policy

**exarch's sandbox is ral's [[design/grant|grant]]: `policy/` composes the
CLI flags into a frozen `GrantStack`, and the stack's per-check fold is the one
meet that ever runs** ([[map/core/capabilities|capabilities]],
[[decisions/260906_object-not-name|object-not-name]]).

```text
  ceiling = base ◁ extension?
  stack   = [ceiling, restrict₁, …, restrictₙ, deny(restricts)?, deny(credentials)?, self?]
```

## Composition — `policy.rs`

`for_invocation(cwd, scratch, base_name, extension, restrict_files, own)`
builds a `FreezeCtx` over the cwd and scratch, reads
`provider::credential_files()`, and calls `compose`. It returns
`Invocation { stack, restricts }`: the invocation's own layers and the
restrict files' absolute paths. `lib.rs::run` passes
`sandbox::pinned_arg0()` as `own`.

- **Ceiling.** `resolve_base` loads the named base; an `--extend` file loads as
  a `Fragment` and `GrantLayer::extend` adds it, still one layer (`◁`: an
  allowance never removes authority, an exclusion always wins;
  [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
- **Restricts.** Each `--restrict` file is pushed as its own layer.
- **Deny layers**, only when `!stack.fs_is_top()`: one over the restrict files'
  own paths (if any), then one over exarch's and synod's credential files
  ([[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]]).
  As separate layers no profile can omit them and no `--extend` can widen them.
  The `--extend` file is not denied.
- **Self layer**, only when `!stack.exec_is_top()`: `self_layer` excludes the
  binary `own` names from exec, so no agent of a restricted session, nor any
  child, starts exarch. `own: None` here refuses as
  `PolicyFailure::OwnBinaryUnpinned`.
- `dangerous` with no restrict restricts no axis: no layer is added and no
  launch needs an OS sandbox.

`deny_layer` and `self_layer` are constructors: each builds typed `Axes`, and
`carve_out` encodes them as a `GrantValue`, decodes it with `decode_fragment`
and returns `GrantLayer::root().extend(fragment)` — a grant value decoded by
the one front end
([[invariants/a-grant-is-decoded-where-it-lands|a-grant-is-decoded-where-it-lands]]).
Every path is held by `freeze::host_absolute` first; one it refuses fails the
layer whole, since a deny naming nothing would fail open.

## Loading — `policy/load.rs`, `policy/base.rs`

- Profiles load through `ral_core::shell::load::profile::load_capabilities_from_*`
  (`load_fragment_from_*` for `--extend`), the surface of ral's
  `--capabilities`. That runs `evaluate_closed` with the
  `typecheck::table::declared(Form::Grant)` options table: a profile is a
  closed unit checked and run under Σ, seeing no session bindings.
- Places (`` `home ``, `` `xdg-* ``, `` `cwd ``, `` `scratch ``, `` `gitdir ``)
  and `dir` selectors resolve as the profile loads, through
  `ral_core::shell::grant`'s decoder; composition sees only resolved
  `GrantLayer`s ([[design/capability-freeze|capability-freeze]]).
- `load` puts a refusal under the flag that named the file, keeping its hints;
  an `exit` or stop inside a profile becomes `PolicyFailure::EscapedLoad`.
  `absolute_in` anchors a command-line path at the cwd.
- The decoder drops dead grants: a bare exec name found neither on `PATH` nor
  among the platform's bundled tools (`name_rules`), and a path rooted in a
  foreign platform's convention.

## Spawned children

`base_layer(base_name, ctx)` is exarch's `GrantNarrower`, installed as the
engine installer's `narrow`. It resolves a base against the child's cwd and
scratch.

- A spawn's grant is one layer
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]):
  `SpawnGrant::layer` (`core/src/shell/grant/spawn.rs`) maps `Inherit` to ⊤,
  `Base(name)` to the narrower, and `Restrict(record)` to `decode_grant`.
- `enquiry.rs::Grant::decode` admits `` `inherit ``, the spawnable bases and
  `` `restrict R `` with `R` a map; `Base::spawnable` excludes `dangerous`
  (⊤ is spelled `` `inherit ``) and `minimal` (it names no bundled tool).
- The desk's `child_engine` (`agent/desk/agents.rs`) hands the `Marshalled`
  parent to `IdentityTransport::child` or `Attach::child`; core's
  `Shell::child_of` reads the image with the parent's stack and pushes the
  grant's layer with `SpawnGrant::narrow_onto`
  ([[map/core/transport|transport]]).
- The fold ANDs every layer, so a spawn only narrows: a network-off parent
  stays offline under a child's `minimal`. A spawn's `R` gets no self-deny
  layer; it has no file on disk.

## The six bases — `exarch/data/*.exarch.ral`

Embedded by `include_str!`; `Base` lists them loosest first.

| base | fs write | net | gpu | exec |
|---|---|---|---|---|
| `dangerous` | ⊤ | ⊤ | ⊤ | ⊤ (`return ()`) |
| `reasonable` (default) | `cwd`, `scratch`, `gitdir` | on | on | `dir `path`, every `dir` selector, `cwd`, `scratch`, everyday names |
| `edit-only` | `cwd`, `scratch` | on | withheld | `dir` `xdg-bin`, `system`, `homebrew`, `/usr/local/bin` and the sbin dirs, everyday names; no writable dir; denies `git` |
| `read-only` | `scratch` | on | withheld | `edit-only`'s dirs plus `cwd`, everyday names; denies `kill` |
| `minimal` | `cwd`, `scratch` | on | withheld | `dir `system`, `cwd`, `scratch` |
| `confined` | `cwd`, `scratch` | offline | withheld | `dir `system`, `dir `homebrew`, `/usr/local/bin`, `cwd`, `scratch`, bundled names |

- **Every spawnable base names each bundled tool** (`core/src/uutils.rs`):
  `Command::from_name` makes one a `Program::Tool` with no path, so no `dir`
  prefix matches it. `every_grantable_base_settles_the_bundled_tools` pins this
  per build configuration.
- `minimal`, `read-only` and `edit-only` deny `bash`, `zsh`, `cmd`,
  `powershell` and `pwsh` by name; `sh` stays admitted.
- `reasonable`, `read-only` and `edit-only` read the XDG dirs and named
  toolchain caches and deny `xdg-config` `gh`, `op`, `gcloud`; `reasonable` also
  denies `~/.dotnet/corefx`.
- No base writes `xdg-cache`; exarch points `XDG_CACHE_HOME`, `TMPDIR` and the
  legacy tool homes into the scratch ([[map/exarch|exarch]]).
- Where the engine host cannot withhold the GPU, the prompt says so.

Tests in `policy/base.rs` pin these per base: `dangerous_is_root`,
`confined_is_offline_subpath_only_no_home_reads`,
`read_only_does_not_write_cwd`, `reasonable_lists_every_selector`,
`the_narrow_bases_withhold_the_gpu`, `reasonable_and_read_only_admit_git`,
`minimal_admits_system_git_not_homebrew`,
`every_exec_base_admits_live_system_tool_roots`. `policy.rs`'s
`the_default_boot_grant_reads_as_pinned` pins the boot grant the agent reads
against `policy/golden/default-boot-grant.md`.
`exarch/examples/git.exarch.ral` is the canonical `--extend`;
`git_extension_extends_into_git_capable_profile` pins that extending `minimal`
with it admits `git` and `~/.gitconfig`, and that extending `reasonable`
keeps its credential denies (deny sets union).

The prompt prints each layer in `grant` syntax
([[map/exarch|exarch]], [[invariants/a-grant-has-one-outward-form|a-grant-has-one-outward-form]]).
The host reads authority only through core's accessors
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]).
