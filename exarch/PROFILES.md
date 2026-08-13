# exarch capability profiles

Every program exarch runs is evaluated under a capability profile. The
`--base` flag chooses one of the six profiles built into the binary;
`reasonable` is the default.

A profile controls:

- which programs may run;
- which paths may be read or written;
- whether the network is available.

The host installs this boundary around every run. The model can use the
authority it receives, but it cannot widen it.

## Choose a profile

- **For ordinary coding:** use `reasonable`.
- **For patches with a smaller tool surface:** use `edit-only`.
- **For review or investigation without project changes:** use `read-only`.
- **For a small base you intend to extend yourself:** use `minimal`.
- **For an offline build or transformation:** use `confined`.
- **For a VM, container, or wholly custom restriction:** use `dangerous`.

These profiles overlap; they are not a single scale from “safe” to “unsafe”.
For example, `read-only` protects the project from writes but keeps the network
on, while `confined` turns the network off but allows project writes.

| profile      | network | project       | reads beyond the project                  | executable surface                                  |
|--------------|---------|---------------|-------------------------------------------|-----------------------------------------------------|
| `dangerous`  | inherit | inherit       | inherit                                   | inherit                                             |
| `reasonable` | on      | read + write  | broad config, state, and toolchain caches | `PATH`, system roots, user toolchains, named tools  |
| `edit-only`  | on      | read + write  | broad config and toolchain caches         | system and editing tools; Git denied                |
| `read-only`  | on      | read only     | broad config and toolchain caches         | system and review tools, including Git              |
| `minimal`    | on      | read + write  | none; `xdg:cache` is writable             | system roots, project, and scratch                  |
| `confined`   | off     | read + write  | none                                      | system roots, `/usr/local/bin`, project, and scratch |

### `reasonable` — everyday coding

This is the default: broad enough to edit code, run tests, fetch dependencies,
and use installed development toolchains.

- **Reads:** the project and Git directory, scratch, XDG config/data/cache/state
  directories, common toolchain caches, user toolchains, and macOS
  `~/Library/Caches`.
- **Writes:** the project and Git directory, scratch, and `xdg:cache`. Persistent
  config, data, state, user binaries, SSH keys, and system directories are not
  writable.
- **Programs:** commands resolved through `PATH`, platform tool roots, common
  user-toolchain directories, project and scratch executables, and a named set
  of everyday tools.
- **Network:** on.

`bash`, `zsh`, and their Windows counterparts are denied; `sh` remains
available for tools such as `configure` and `make`.

Git works locally, including unsigned commits. SSH and GPG keys are not
readable, so SSH pushes and signed commits fail unless you deliberately extend
the profile. On macOS, an HTTPS push may still use credentials held by the
Keychain because that service is reached outside the filesystem.

`reasonable` is a practical coding boundary, not a secrecy boundary around the
whole home directory. It deliberately reads broad XDG config, data, and state
surfaces. Known credential directories for `gh`, 1Password, and Google Cloud
are denied, but if other home data must remain private, start from `minimal` or
add a restriction.

### `edit-only` — patch and refactor

Use `edit-only` when the agent should change source files with fewer developer
tools available than `reasonable`.

- It reads the project, common XDG directories, and common toolchain caches.
- It writes only the project and scratch.
- It keeps the network on for remote context.
- It admits system tools, project and scratch executables, Python, search,
  patch, archive, and network utilities.
- It denies Git and interactive shells.

The name describes the intended job, not a proof that no build can execute: a
build tool under a system root, or an executable already in the project, may
still run. If “no builds” is a hard requirement, add an explicit `--restrict`
file for the executable surface you want.

### `read-only` — review and investigate

`read-only` can inspect the project and run review tools, but it cannot modify
the project tree. Writes are limited to scratch.

Git is available for `log`, `show`, and `diff`; commands such as `commit` fail
at the filesystem boundary. Tools that insist on writing into the project may
also fail and should be pointed at `$EXARCH_SCRATCH` where possible.

The profile still has network access and broad reads of config and toolchain
caches. “Read-only” describes project mutation, not confidentiality or
network isolation.

### `minimal` — a small base to extend

`minimal` is the smallest useful starting point for a custom profile.

- **Reads:** project and scratch only.
- **Writes:** project, scratch, and `xdg:cache`.
- **Programs:** platform system roots, project executables, and scratch
  executables.
- **Network:** on.

It does not grant reads of home-directory config, XDG data, or toolchain
caches. Interactive shells are denied, although `sh` remains available. On
Unix, a Git binary under a system root may run, but Git config is not readable;
the supplied extension adds Git config and admits Git wherever it is installed:

```text
exarch --base minimal --extend-base exarch/examples/git.exarch.ral
```

`minimal` explicitly excludes `/opt/homebrew`; other system roots remain
platform-dependent.

### `confined` — offline build jail

`confined` is for compiling or transforming one tree without network access.
It is shaped after
[BrianSwift/macOSSandboxBuild's `confined.sb`][confined-sb].

- **Reads and writes:** project and scratch only.
- **Programs:** platform system roots, `/usr/local/bin`, project executables,
  scratch executables, and the coreutils built into the binary.
- **Network:** off.

`rm`, `mv` and `truncate` are admitted like any other tool. What a confined
agent can destroy is settled by the write paths above, which stop at the project
and scratch; withholding the commands as well would break `make clean` without
protecting a single byte the path check does not already hold.

The platform supplies runtime and toolchain paths needed by the sandbox. On
macOS, that includes Command Line Tools and Xcode, so a compiler can reach its
assembler and linker without granting the rest of the home directory.

[confined-sb]: https://github.com/BrianSwift/macOSSandboxBuild/blob/master/confined.sb

### `dangerous` — ambient authority

`dangerous` applies no restriction. Agent programs receive the same ambient
authority as commands typed at your own prompt.

Use it when another boundary already exists, such as a disposable VM or
container. It is also the starting point for a profile written entirely as a
restriction:

```text
exarch --base dangerous --restrict mine.ral
```

## Compose a profile

An optional `--extend-base` file widens the selected base. Any number of
`--restrict` files then narrow it:

```text
effective = (base ⊔ extension) ⊓ restriction₁ ⊓ restriction₂ ⊓ …
```

Each file is frozen as it loads: `~`, `xdg:`, `cwd:`, `tempdir:`, `gitdir:`,
and `system:` resolve to fixed paths for that session before the capabilities
are composed. Changing the environment later cannot move those grants.

Two of them are read from a source the session does not author, and each is
checked rather than trusted. An `xdg:` path must land under the home directory.
A `gitdir:` in a worktree or submodule — where `.git` is a file naming the real
Git directory — is followed only as far as a Git directory that names the
working tree back, since that file sits in the tree the agent may write. A
pointer nothing claims refuses the session and names both paths.

A restriction file is itself added to the filesystem deny set, so the agent
cannot rewrite the file that defines its boundary.

A typical custom setup starts small, adds trusted build tools, then confines
the result to the project:

```text
exarch --base minimal --extend-base build-tools.ral --restrict project.ral
```

## Platform tool roots

The `system:` name expands to the platform's live tool roots:

- `/usr/bin` and `/bin`, plus detected Homebrew or Linuxbrew roots, on Unix;
- `%SystemRoot%\System32`, Windows PowerShell, and Git for Windows' `usr\bin`
  when present, on Windows.

macOS also supplies Command Line Tools and Xcode paths at the sandbox layer.
Unix-only path entries are discarded on Windows rather than making the whole
profile fail to load. `policy show` therefore reports only grants the current
platform can back.

## Build-tool caches

`reasonable` and `minimal` permit writes to `xdg:cache`, so modern tools that
honour `$XDG_CACHE_HOME` use an allowed cache automatically.

For common tools that use older home-directory conventions, exarch redirects
these variables into `$EXARCH_SCRATCH` when the session starts:

| variable           | tool   | scratch directory |
|--------------------|--------|-------------------|
| `CARGO_HOME`       | Cargo  | `cargo`           |
| `npm_config_cache` | npm    | `npm-cache`       |
| `GRADLE_USER_HOME` | Gradle | `gradle`          |
| `GOPATH`           | Go     | `go`              |
| `GOMODCACHE`       | Go     | `go/pkg/mod`      |
| `RUSTUP_HOME`      | rustup | `rustup`          |

These values replace inherited ones. A build therefore writes to disposable
scratch instead of silently targeting a real cache that the active profile
does not permit.

## Enforcement

ral checks executable authority before spawning on every platform. Filesystem
and network restrictions are also projected into the platform sandbox:
Seatbelt on macOS, bubblewrap with seccomp on Linux, and an AppContainer
LowBox token on Windows.

This is defence in depth for a development tool, not a claim that exarch is a
hardened jail.

## Where the built-ins live

The six profiles are ral programs embedded into the exarch binary:

```text
exarch/data/dangerous.exarch.ral
exarch/data/reasonable.exarch.ral
exarch/data/edit-only.exarch.ral
exarch/data/read-only.exarch.ral
exarch/data/minimal.exarch.ral
exarch/data/confined.exarch.ral
```

Custom profiles do not need to live in a special directory. Pass a ral file to
`--extend-base` or `--restrict`.
