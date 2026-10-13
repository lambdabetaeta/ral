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
| `minimal`    | on      | read + write  | none; `` `xdg-cache `` is writable             | system roots, project, and scratch                  |
| `confined`   | off     | read + write  | none                                      | system roots, Homebrew, `/usr/local/bin`, project, and scratch |

### `reasonable` — everyday coding

This is the default: broad enough to edit code, run tests, fetch dependencies,
and use installed development toolchains.

- **Reads:** the project and Git directory, scratch, XDG config/data/cache/state
  directories, common toolchain caches, user toolchains, and macOS
  `~/Library/Caches`.
- **Writes:** the project and Git directory, scratch, and `` `xdg-cache ``. Persistent
  config, data, state, user binaries, SSH keys, and system directories are not
  writable.
- **Programs:** commands resolved through `PATH`, platform tool roots, common
  user-toolchain directories, project and scratch executables, and a named set
  of everyday tools.
- **Network:** on.

Interactive shells are not singled out: `bash` and `zsh` are admitted through
the same `PATH` and system-root entries as any other binary. What a shell can
do here is what this profile allows, so denying it by name would steer rather
than confine. Use `minimal`, which denies both, when the ban must hold.

Git works locally, including unsigned commits. SSH and GPG keys are not
readable, so SSH pushes and signed commits fail unless you deliberately extend
the profile. On macOS, an HTTPS push may still use credentials held by the
Keychain because that service is reached outside the filesystem.

`reasonable` is a practical coding boundary, not a secrecy boundary around the
whole home directory. It deliberately reads broad XDG config, data, and state
surfaces. Known credential directories for `gh`, 1Password, and Google Cloud
are denied, but if other home data must remain private, start from `minimal` or
add a restriction. exarch's and synod's own credential files — the keychain
fallback and the ChatGPT OAuth tokens under the XDG config and state homes —
are denied too, by every profile that restricts the filesystem, so an agent
cannot read the key that pays for its own session.

### `edit-only` — patch and refactor

Use `edit-only` when the agent should change source files with fewer developer
tools available than `reasonable`.

- It reads the project, common XDG directories, and common toolchain caches.
- It writes only the project and scratch.
- It keeps the network on for remote context.
- It admits system tools, Python, search, patch, archive, and network
  utilities, but no executable in the project or scratch: a directory that is
  both writable and exec-admitted can be handed a copy of a denied tool under
  another name, and nothing in ral freezes it against that (see
  [What exclusions cannot do](#what-exclusions-cannot-do)).
- It denies Git and interactive shells.

The denies hold only where the project's write region covers no directory the
profile runs. Launched from the project directory that is so. Launched from
`~`, `` `cwd . `` is the home directory, which covers `` `dir `xdg-bin . ``
(`~/.local/bin`): that directory is runnable and now writable, so a child can
author a program there, including one you later run from your own `PATH`
outside any sandbox, and the Git and shell denies there become advisory to
descendants. Launch `edit-only` from a project directory.

To see which folders a profile leaves both writable and runnable, push it as
a session layer under an open audit trail; the trail then holds one `deputy`
check per finding:

```text
ral --audit --capabilities exarch/data/edit-only.exarch.ral -c true
```

Run from `~`, this records a `deputy` check for `~/.local/bin`.

The name describes the intended job, not a proof that no build can execute: a
build tool under a system root still runs. If “no builds” is a hard
requirement, add an explicit `--restrict` file for the executable surface you
want.

### `read-only` — review and investigate

`read-only` can inspect the project and run review tools, but it cannot modify
the project tree. Writes are limited to scratch.

Git is available for `log`, `show`, and `diff`; commands such as `commit` fail
at the filesystem boundary. Tools that insist on writing into the project may
also fail and should be pointed at `$EXARCH_SCRATCH` where possible — which is
writable but not exec-admitted, so a binary written there cannot then be run.

The profile still has network access and broad reads of config and toolchain
caches. “Read-only” describes project mutation, not confidentiality or
network isolation.

### `minimal` — a small base to extend

`minimal` is the smallest useful starting point for a custom profile.

- **Reads:** project and scratch only.
- **Writes:** project, scratch, and `` `xdg-cache ``.
- **Programs:** platform system roots, project executables, and scratch
  executables.
- **Network:** on.

It does not grant reads of home-directory config, XDG data, or toolchain
caches. Interactive shells are denied, although `sh` remains available. On
Unix, a Git binary under a system root may run, but Git config is not readable;
the supplied extension adds Git config and admits Git wherever it is installed:

```text
exarch --base minimal --extend exarch/examples/git.exarch.ral
```

`minimal` leaves Homebrew unlisted; add brew tools with `--extend` (a file
allowing `` `dir `homebrew ``). Other system roots remain platform-dependent.

### `confined` — offline build jail

`confined` is for compiling or transforming one tree without network access.
It is shaped after
[BrianSwift/macOSSandboxBuild's `confined.sb`][confined-sb].

- **Reads and writes:** project and scratch only.
- **Programs:** platform system roots, `` `dir `homebrew `` where Homebrew is
  present, `/usr/local/bin`, project executables, scratch executables, and the coreutils built into the binary.
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
container.

It is ambient authority less starting exarch. In every session, `dangerous`
included, exarch pushes a last layer that excludes its own binary from
execution, by the path it was started from; every spawned agent inherits it.
The agent loses exarch as a process (by path, by a name that resolves to it,
or through `sh -c`); it keeps evaluating ral in its own shell, and bundled
tools still run. Because that layer restricts, a `dangerous` session is
confined like any other: launch hardening applies, and on Linux and macOS
every launch runs in the sandbox (a profile that allows everything and
refuses that one binary). On Linux a program created during a launch directly
in a directory on the path to exarch's binary (`/`, `~`, `~/.cargo`,
`~/.cargo/bin` for a cargo install) runs from the next launch on. The
exclusion is a guardrail: another install of exarch or ral in a folder the
grant admits still runs, as does a copy in a folder that is both writable and
runnable; on Windows, which has no kernel exec layer, it is the in-process
guard's alone.

`dangerous` is also the starting point for a profile written entirely as a
restriction:

```text
exarch --base dangerous --restrict mine.ral
```

## Compose a profile

An optional `--extend` file extends the selected base once; every later layer
restricts. The file's allowances and exclusions join the base's before any
`--restrict`:

- an allowance never removes permission, on any axis;
- an exclusion always wins, over every allowance and regardless of how
  specific the allowance is: an exact file or a deeper directory inside an
  excluded directory does not run, and neither does a carrier or the dynamic
  linker;
- an area the base leaves unrestricted stays so, less the file's exclusions:
  `--base dangerous --extend git.exarch.ral` stays unrestricted, apart from
  exarch's own binary, and an extension with an exclusion yields "every
  request, less the exclusions";
- verb lists join by union: an extension can add a `shell` or `editor` verb
  the base lacks, a base cannot withhold a verb (`detach` included) against
  an extension, and a base that omits a verb list keeps every verb.

Any number of `--restrict` files then narrow the result. Each is a layer
pushed on a stack, in argument order, and a layer reads the run places of the
layers beneath it, so order matters:

```text
stack = [ base ◁ extension, restriction₁, restriction₂, …, exarch's own layers ]
```

A refusal that cites an exclusion says that an allowance cannot override it
and points at the policy that imposes it; `--extend` cannot lift it.

A launch on Windows is refused when the filesystem region holds every path:
`dangerous` with an exclusion from `--extend`, or with a restriction file that
only excludes and has no allowances (an fs-restricting restriction file also
brings exarch's exclusions of itself and of its credential files). Windows' sandbox reaches only folders stamped for it, and ral will
not stamp every drive; grant the folders the program needs instead. The
in-process guard still judges, and in-process work is unaffected.

Each file is frozen as it loads: the places `` `home ``, `` `xdg-* ``, `` `cwd ``,
`` `tempdir ``, `` `gitdir ``, and the `` `system ``, `` `homebrew `` and `` `path `` selectors resolve to
fixed paths for that session before the capabilities are composed. Changing the environment later cannot move those grants.

Two of them are read from a source the session does not author, and each is
checked rather than trusted. An `` `xdg-* `` place must land under the home directory.
A `` `gitdir `` in a worktree or submodule — where `.git` is a file naming the real
Git directory — is followed only as far as a Git directory that names the
working tree back, since that file sits in the tree the agent may write. A
pointer nothing claims refuses the session and names both paths.

A restriction file is itself added to the filesystem deny set, so the agent
cannot rewrite the file that defines its boundary. exarch's and synod's own
credential files sit on the same footing: denied wherever the effective grant
restricts the filesystem at all, so an agent can never read the key that pays
for its own turn.

A typical custom setup starts small, adds trusted build tools, then confines
the result to the project:

```text
exarch --base minimal --extend build-tools.ral --restrict project.ral
```

## Platform tool roots

The `` `dir `system `` target expands to the platform's live tool roots:

- `/usr/bin` and `/bin`, plus the toolchain roots present, on Unix; Homebrew is
  the separate `` `dir `homebrew `` selector (`HOMEBREW_PREFIX`, else
  `/opt/homebrew` if it exists);
- `%SystemRoot%\System32`, Windows PowerShell, and Git for Windows' `usr\bin`
  when present, on Windows.

macOS also supplies Command Line Tools and Xcode paths at the sandbox layer.
Unix-only path entries are discarded on Windows rather than making the whole
profile fail to load. The audit trail (`ral --audit --capabilities FILE -c true`) therefore
reports only grants the current platform can back.

## Build-tool caches

`reasonable` and `minimal` permit writes to `` `xdg-cache ``, so modern tools that
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

One further variable is forced, though it relocates nothing:

| variable                       | tool  | value  |
|--------------------------------|-------|--------|
| `CARGO_NET_GIT_FETCH_WITH_CLI` | Cargo | `true` |

Cargo's bundled git speaks TLS through macOS SecureTransport, whose handshake
reaches securityd — the keychain door no profile admits — so a git dependency
otherwise fails as `ssl handshake -9808`, which cargo reports as a missing
revision. The `git` binary's TLS needs no such door, so cargo fetches through
it. A profile that denies `git`, such as `edit-only`, therefore cannot resolve
git dependencies at all.

## Reading, writing and running

- **Running and writing read.** Whatever a grant lets run by path, directory or
  name, or lets be written, may be read. A grant reads what it, and the grants
  enclosing it, let run; a nested grant never makes something readable by
  naming it in `exec`. So a restriction that supplies filesystem alone does not
  hide the tools an enclosing grant runs: a restrict file reading only
  `` `cwd . `` leaves `cargo` in `~/.cargo/bin` runnable. To hide one, exclude
  it in `fs`; an `fs` exclusion of a run place refuses running it.
- **Programs that run through "every program"** (an omitted `exec`) must be
  readable under `fs`: an unreadable program outside the reads is refused.
- **Allowances keep what they named.** A path allowance, to read, write or run,
  names the real path it resolved to when the profile loaded. A symlink put in
  its place later redirects nothing, and a launch whose allowed folder, or an
  ancestor, was replaced by a symlink is refused on Linux and Windows, naming
  it (it reaches nothing on macOS). Creating a granted directory after the
  grant, or `rm -rf dir && mkdir dir`, keeps working. Exclusions follow their
  names, so they catch whatever the name reaches now; a bare program name
  follows `PATH` afresh at each run. A grant whose real path is not valid
  Unicode is refused when loaded.
- **Interpreters under filesystem-only restrictions.** A trusted interpreter
  outside the reads starts its scripts under a restriction that supplies
  `fs` alone, as it already did under a restricted `exec`.
- **ral itself is an ordinary file.** `ral -c`, `#!` ral scripts and
  `sh -c 'ral …'` run exactly where a grant admits ral, in the guard and the
  kernel; under an `exec` allow-list that omits it they are refused. Where a
  grant admits ral's folder (`reasonable` admits `~/.cargo/bin/`), a nested ral
  runs bound by the kernel alone: bundled-tool rules and verbs do not bind it.
  An `fs` exclusion of ral's binary refuses every bundled tool, which runs
  ral's image; an `exec` exclusion leaves bundled tools alone.
- **Every program, less the exclusions.** On Linux, a program created during a
  launch on the path to an excluded path does not run until the next launch,
  and under a restricted filesystem the sandbox's private `/tmp` runs nothing:
  `dangerous` with a restrict file that restricts the filesystem cannot run a
  program built in `/tmp`.
- **Activation reads what a policy means.** A layer that restricts nothing,
  such as a filesystem that writes the root and excludes nothing, activates no
  launch hardening. exarch pushes its credential layer, and the exclusion of
  each `--restrict` file, only where some filesystem restricts: `--base
  dangerous --extend` with a positive-only `fs` gets none, and with an `fs`
  exclusion it does. A restriction file that leaves `fs` silent is therefore
  editable by the agent, as an `--extend` file is.

## What exclusions cannot do

An exclusion names programs by path and name. It is a guardrail, not a wall:
a copy of a program, a newly written program or a script fed to an admitted
interpreter is a different request. ral no longer freezes a run-admitted
folder that a broad write covers, with or without an exclusion; the freeze
made an allowance withdraw write authority in the kernel alone. Any broad
write over a run-admitted folder now lets a child write programs there,
including ones you later run from your own `PATH` outside any sandbox, and a
name exclusion there is advisory to descendants. The deputy findings name every
such folder, and now list each writable island under a runnable folder (not
a folder wholly excluded on either side); they are recorded only into an audit
trail, so read them with `audit` as `edit-only` above describes. Other limits:
a hard link already inside a granted region to a file outside it reads or
writes that file; on Linux an excluded path that is a hard link to an admitted
file runs; the kernel sees no argv; Windows has no kernel exec layer.

## Enforcement

ral checks executable authority before spawning on every platform. Filesystem
and network restrictions are also projected into the platform sandbox:
Seatbelt on macOS, bubblewrap with seccomp on Linux, and an AppContainer
LowBox token on Windows.

This is defence in depth for a development tool, not a claim that exarch is a
hardened jail.

One deny does not reach the Linux sandbox layer: a path that does not exist
when the session starts, under a prefix the profile makes writable. bubblewrap
can only deny a path by mounting over it, and mounting over a name that is not
there would have it create that name in your own tree first. ral's own checks
still refuse the path; a command spawned under the grant can create it. Denying
a file that already exists, or one under a read-only prefix, is enforced by
both layers, as it is on macOS either way.

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
`--extend` or `--restrict`.
