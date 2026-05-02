# exarch capability profiles

exarch wraps every model-emitted command in a `grant` block.  The
`--base` flag selects the ceiling for that grant.  Five bake-ins
ship in the binary, in descending order of authority:

| profile      | net | reads                                              | writes                          | exec                                                                  |
|--------------|-----|----------------------------------------------------|---------------------------------|-----------------------------------------------------------------------|
| `dangerous`  | —   | inherit (no attenuation)                           | inherit                         | inherit                                                               |
| `reasonable` | on  | broad: cwd + xdg:* + toolchain caches + ~/Library  | cwd + scratch + xdg:cache       | system bins + xdg:bin + /opt/homebrew + curated named tools           |
| `read-only`  | on  | same as reasonable                                 | scratch + xdg:cache only        | same as reasonable                                                    |
| `minimal`    | on  | cwd + scratch                                      | cwd + scratch + xdg:cache       | `/bin/` + `/usr/bin/` + cwd + scratch                                 |
| `confined`   | off | cwd + scratch                                      | cwd + scratch                   | `/bin/` + `/usr/bin/` + `/usr/local/bin/` + cwd + scratch             |

Apple's toolchain (`/Library/Developer/CommandLineTools`,
`/Applications/Xcode.app/Contents/Developer`, `/opt/homebrew`) is
folded into every profile's exec admit at the OS sandbox layer
automatically — see `core/src/sandbox/macos.rs::system_paths`.  No
profile needs to spell out where ld and as live.

## What to use when

```
need network?
├─ no  → confined
└─ yes → need to write outside cwd?
         ├─ no  → read-only
         └─ yes → which tooling surface?
                  ├─ system tools only        → minimal
                  ├─ system + brew + xdg:bin  → reasonable
                  └─ paranoid / custom        → dangerous + --restrict
```

### `dangerous` — escape hatch / lattice top

No attenuation.  ral runs with full ambient authority, equivalent to
typing the commands yourself.  Used in two ways:

- **Pre-sandboxed environment** — running exarch inside a Docker
  container or VM, where the container is the trust boundary and
  in-process attenuation is redundant.  This is the default in
  `exarch/docker/entrypoint.sh`.
- **Paranoid custom profile** — combine with `--restrict <FILE>` to
  start permissive and meet down to your hand-written allow-list:
  `exarch --base dangerous --restrict mine.toml`.  Every entry in
  the effective profile traces back to text you wrote.

### `reasonable` — everyday agent (default)

The default.  Designed for an agent you don't fully trust (may
hallucinate) but want to give maximal flexibility for real work.

- Network on.
- Reads: cwd + xdg config / data / cache / state / bin + most
  toolchain caches (`~/.cargo/registry`, `~/.rustup`, `~/.npm`,
  `~/.gradle/caches`, `~/.m2/repository`, …) + `~/Library/Caches`.
- Writes: **only ephemeral / recoverable surfaces** — cwd, /tmp,
  `tempdir:`, `xdg:cache`.  A hallucinating agent cannot corrupt
  persistent state (`xdg:config`, `xdg:data`, `xdg:state`, `~/.ssh`,
  `xdg:bin`, system dirs).
- Exec: 80+ named tools (coreutils, git, curl, gh, rg, fd, jq,
  python, tar, …) plus subpath admits for `/usr/bin/`, `/bin/`,
  `/usr/local/bin/`, `/opt/homebrew/bin/`, `/usr/sbin/`, etc.
  `bash` and `zsh` explicitly denied — `sh` itself is allowed
  because autoconf-style `configure` shells out via `/bin/sh -c`.
- Credential dirs (`xdg:config/gh`, `xdg:config/op`,
  `xdg:config/gcloud`) denied even for read.

Use for: editing code, running tests, fetching dependencies,
"download and build neovim," day-to-day developer-assistant work.

### `read-only` — review / audit / investigate

Same reads and exec admits as `reasonable`, but `cwd:` is **not**
in `write_prefixes`.  Writes go only to scratch and `xdg:cache`.

Use for: code review agents, "explain this repo," static analysis
flows, "why is this build failing," any task where the agent
should observe but not modify the project tree.

### `minimal` — additive starting point

Smallest profile that's actually useful as a base for
`--extend-base` composition.

- Network on.
- Reads: cwd + scratch only — no `xdg:*`, no `~/.cargo`, no
  `~/Library`, nothing user-installed.
- Writes: cwd + scratch + `xdg:cache`.
- Exec: `/bin/` + `/usr/bin/` (covers coreutils, sh, make, find,
  xargs, awk, sed, clang, pkg-config, …) + cwd + scratch.  No
  `xdg:bin`, no `/opt/homebrew`.  `bash` and `zsh` denied.

Use for: "I want my agent to operate on this tree with the standard
system tools, nothing it pulled from my home directory or homebrew,
network on so it can fetch what I tell it to fetch."

Typical pattern:

```
exarch --base minimal --extend-base build-tools.toml --restrict project.toml
```

where `build-tools.toml` adds whatever specific tools you trust
(`/opt/homebrew/bin/`, `cargo`, `pip`, …) and `project.toml`
confines fs to one subtree.

### `confined` — build jail

Shaped after [BrianSwift/macOSSandboxBuild's `confined.sb`][confined-sb].
Tight build-and-nothing-else profile.

- Network **off**.
- Reads: cwd + scratch.
- Writes: cwd + scratch.
- Exec: `/bin/` + `/usr/bin/` + `/usr/local/bin/` + cwd + scratch
  (subpath-only, no per-name lattice).

Apple's toolchain comes for free via the OS sandbox base, so
`gcc → cc1 → as → ld` resolves end-to-end.

Use for: agents whose job is to compile or process *this one tree*
and nothing else.  Source already on disk; no fetching from
upstream; no leakage.

[confined-sb]: https://github.com/BrianSwift/macOSSandboxBuild/blob/master/confined.sb

## Composition

Profiles compose via `--extend-base` (load-time *join*; widens) and
`--restrict` (meet; narrows).  The orchestrator runs all
composition before a single freeze pass settles every `~` /
`xdg:` / `cwd:` / `tempdir:` sigil, so paths in your profile bind
to one fixed location at session start — later env mutation can't
widen authority retroactively.

```
effective = base ⊔ extend_base ⊓ restrict₁ ⊓ restrict₂ ⊓ …
```

`--restrict` files are also added to the fs deny list, so the
agent can never modify the input that shaped its own permissions.

## Cache redirection (legacy build tools)

`reasonable` and `minimal` admit `xdg:cache` for write, so any
tool that respects `$XDG_CACHE_HOME` (uv, pnpm, bun, mise, ruff,
hatch, deno, modern python, …) lands its cache writes inside the
allowed surface automatically.

Six legacy build tools that pre-date or ignore XDG get explicit
home-env redirection at session entry, so their caches land in
`$EXARCH_SCRATCH/<tool>` instead of `~/.cargo`, `~/.npm`, etc.:

| env var            | tool   | scratch sub  |
|--------------------|--------|--------------|
| `CARGO_HOME`       | cargo  | `cargo`      |
| `npm_config_cache` | npm    | `npm-cache`  |
| `GRADLE_USER_HOME` | gradle | `gradle`     |
| `GOPATH`           | go     | `go`         |
| `GOMODCACHE`       | go     | `go/pkg/mod` |
| `RUSTUP_HOME`      | rustup | `rustup`     |

Always overrides — the sandbox is the trust boundary, not the
inherited environment.  A user pre-set `CARGO_HOME` pointing into
`~/.cargo` would land outside the write set and fail loudly inside
the agent, so we replace it.

## OS-level enforcement

The `grant` block produces a `SandboxProjection`; on macOS the
projection renders to a Seatbelt SBPL profile, on Linux to a
bubblewrap argv with a seccomp BPF filter.  See
`core/src/sandbox/macos.rs` and `core/src/sandbox/linux.rs`.  The
in-process capability check fires on every platform; OS-level
enforcement is depth-in-defence for fs+net (and on macOS, also for
exec — closing the `sh -c "PATH=…; cmd"` interpreter-bypass class).

## Where the profiles live

Each profile is a TOML file in `exarch/data/`, embedded into the
binary at build time via `include_str!`:

```
exarch/data/dangerous.exarch.toml
exarch/data/reasonable.exarch.toml
exarch/data/read-only.exarch.toml
exarch/data/minimal.exarch.toml
exarch/data/confined.exarch.toml
```

There is no directory convention for adding more — bases are
bake-ins.  To use your own profile from disk, write a TOML file
and pass it via `--restrict` or `--extend-base` against one of the
five built-ins.
