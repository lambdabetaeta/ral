---
generated_at_commit: 9bdbb945
generated_at_date: 2026-10-06
covers_paths: [core/src/capability/, core/src/capability.rs, core/src/sandbox/, core/src/sandbox.rs, core/src/path/, core/src/path.rs]
---

# Map: core / capabilities & sandbox

The [[design/grant|grant]] mechanism in two halves: an in-process decision layer
and an OS process sandbox that enforces it for external commands — each
authoritative exactly where the other is blind ([[design/two-enforcers|two
enforcers]]). Authority is attenuated by intersection — a `grant` block can only
narrow.

## Decision layer — `core/src/capability/`

Every runtime yes/no over the dynamic capability stack is a free
`capability::check_*(&Context, …)` function that folds the whole stack
(`ctx.grants`): `admits_head`, `check_exec`, `check_fs_op` (a read by
name) and `check_fs_exact` (the object `Shell::locate` walked to), the
editor/shell bool checks, and the OS-renderable `sandbox_projection`. The
fold combines the layers' *verdicts*; there is no `Capabilities::meet`
([[decisions/260906_object-not-name|object-not-name]]). The
`capability` module is the only place authority is decided — a module boundary
rather than a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). Why `Capabilities`,
the live judgment, and `SandboxProjection` are distinct and not one is argued in
[[design/capability-carriers|capability-carriers]].

Submodules:

- `enforce.rs` — the in-process guards: head admission, the
  audit-bearing exec/fs checks (`check_exec`, `check_fs_op`,
  `check_fs_exact`), and the editor/shell bool checks. `check_exec` returns
  `Admitted` — the program, its argv and the table that judged them, private
  fields, minted nowhere else — which is all a launch reads. The fs guard is split
  so the judgment is reusable without the report: `fs_verdict` is the
  decision — `Guarded` first, for a write onto a boot-pinned sandbox binary
  (`sandbox::pinned_binary`, by inode), before any grant is folded —
  `check_fs_exact` audits it and mints the `Break` for a symlink-free path,
  and `check_fs_op` is the read-by-name layer over it that canonicalises
  leniently, excuses the discard device (`LexicalPath::is_discard`) and
  refuses a reserved device name (`check_device_name`) before either region is
  consulted. `Shell::locate` (`types/shell/checks.rs`) is the door every open
  takes: the same name refusal, `path::walk` to the object, then
  `check_fs_exact` on `Located::real`; `GrantStack::admits_fs_exact` is the
  quiet twin a write asks before reading its before- and after-images;
- `sandbox.rs` — the OS-renderable `sandbox_projection` builder; fs is the
  read and write `Region`s, re-frozen once at spawn, rendered as the surface
  strings of their `live()` allows and their `denies()` (`surface`); exec is
  `ExecRules::kernel` of the `Admitted`'s own table, with the carriers of its
  allowed files and its program (`sandbox::carriers`), so the profile renders
  the table that judged the launch rather than a second compile;
- `deputy.rs` — `deputy_prefixes`, the confused-deputy report: the prefixes a
  grant makes both `exec`-admitted and `fs`-writable, each dimension folded
  across the `GrantStack` as a `Region` of allows and read through `live()`,
  pairs judged by `holds::<Allow>` at the other's own subject (neither layer is
  guilty alone). **What it locates is where a write becomes runnable, not an
  escalation**: within one projection the dropped binary is spawned under the
  confinement that wrote it, and only a runner outside the projection turns
  the shape into an escape — so it reports and never denies. Findings surface at grant push and at an exarch profile load
  ([[design/grant|grant]]);
- `table.rs` — `Table<K: Scope>`, authority as a table of scoped rules
  ([[design/authority-tables|authority-tables]]): `Scope` (`rank`,
  `holds::<P>`, `own`), `verdict` (most specific decides, ties meet, silence
  denies), `live`, `denies`, `respelled`, and `Meet for Table`, denies joining
  and allows meeting at their own subjects; the meet's law and lattice laws are
  its property tests;
- `fs.rs` — the fs instance: `FsOp`, `Region = Table<FrozenPath>`
  (subject a `&Path`, rank `(is a deny, depth)`: a deny outranking every allow, the deeper above among each), and `region`, each
  opining layer's prefixes for the op and its `deny_paths`, re-frozen against
  the caller's `Resolver` (`FrozenPath::refreeze`) and met;
- `exec.rs` — the exec instance
  ([[decisions/261004_exec-rules|exec-rules]]): `Program` (`Tool` or `File {
  path, real }`) and the `Subject` it is judged as (`Tool`, `File`, `Under`);
  `ExecScope` (`Dir`, `Carrier`, `File`, `Tool`, `Name`) ranked by `Rank`
  (`Dir(depth) < Carrier < Exact < Name`); `ExecRules = Table<ExecScope>`;
  `ExecRules::compile`, one layer's `ExecGrant` against this host, a bare key
  resolved on the host `PATH`; `rules`, the stack's table, compiled per
  question; `ExecRules::kernel`, the table with its carriers as `Carrier`
  scopes, rendered in `Rank` order for last-match-wins; `allowed_files`, the
  live file scopes the carriers are computed over. Vetoes key on
  `path::command_name_key`;
- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a frozen `Capabilities`, one dimension
  decoder per `exec` / `fs` / `net` / `detach` / `editor` / `shell` key —
  every one of them authority, none of them a recording switch
  ([[design/audit|audit]]); its
  exec-map freeze expands the two *exec-only* sigils `path:` (every `$PATH`
  component) and `system:` (the platform's tool roots,
  `sigil::system_tool_roots`) into `ExecKey::Dir` entries, every entry through
  `meet_insert`, and drops bundled-tool grants for coreutils a
  host does not ship (`COREUTILS_UNIX_ONLY_TOOLS`);
- `load.rs` — `load_capabilities_from_path` / `_from_str` for `.ral`
  capability profiles, compiled under `grant`'s own declared table
  (`typecheck::contract`, `Form::Grant`): a profile's dimensions *are* the
  form's options, so a returned row is held to them statically and the walker
  above is where a profile returning a map meets the same six keys.

The capability *types* live in [[map/core/shell-state|types/capability]]: the
single always-frozen `Capabilities`, resolved at decode by the freeze pass
inside `decode_capability_map` ([[design/capability-freeze|freeze boundary]]);
plus `FsPolicy`, `GrantStack`,
`Meet`, `Widen`, and the authored exec grant
`ExecGrant(BTreeMap<ExecKey, Verdict>)` — `ExecKey::{Name, Path, Dir}`, bare
keys, frozen path keys and frozen dir keys, under `Verdict`
(`Deny < Only(s) < Allow`), a dir taking only `Allow` or `Deny` since it cannot
restrict argv; every entry added by `meet_insert`, and `Display` spelling a key
as a grant does. It rides the wire (as a pair sequence) and is never matched
directly; `ExecRules` is what judges. The kernel's view is
`ExecProjection::Restricted(Vec<ExecRule>)`, an ordered `Dir`/`File`/`Veto`
list.

Path resolution for grant matching is `core/src/path/`: a fixed staged rule,
plus `which.rs` for PATH search.

- expand — `sigil.rs` (the five path-prefix sigils: `~`, `xdg:`, `cwd:`,
  `tempdir:`, `gitdir:` — the last three policy-only, expanded at freeze;
  `git.rs` backs `gitdir:` discovery, following a `.git` pointer file only to a
  git directory whose `gitdir` back-pointer or `core.worktree` names the working
  tree back), `tilde.rs` (`~` and `~/sub` only; an unset `HOME` is `None`,
  and each caller picks its own honest answer);
- lex — `lex.rs`;
- canonicalise — `canon.rs`, for a read by name;
- locate — `walk.rs`, for an open: `walk` descends from the root through
  directory handles with no symlink followed by the kernel, splicing each
  link into the name and re-walking, and hands back a `Located` — the
  directory handle, the leaf, and the symlink-free `real` path — whose every
  operation is handle-relative with `FollowSymlinks::No` (`cap-primitives`
  supplies the `*at` calls on Unix and Windows). A `..` is physical, the
  kernel's rule, even from a link target (the user's own path keeps its
  logical fold); names are spelled as the disk stores them on macOS
  (`getattrlistat`), so case-variant names meet their deny; the splice count
  is bounded by `MAX_HOPS`, which `carriers::trusted_real` shares;
- match — `lex::path_within`, the form-blind containment kernel, which takes
  an `Identity` and folds it over the alias pairs. `Stored` is bytes, or under
  Windows path semantics `starts_with_identity`, unifying ASCII case, `/` vs
  `\`, and `\\?\`-verbatim spellings; `Collision` compares components by
  `lex::collision_key`, canonical caseless matching of the uppercase image
  (ICU4X), the coarsest identity any filesystem gives a name. A rule's
  `Polarity` (`lex::Allow`, `lex::Deny`) picks `Stored` or `Collision`
  ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]);
  on Windows `walk::dealias` names a `~`-bearing leaf by its long name, and
  refuses one it cannot name;
- name a device — `lex::is_discard_device`, behind
  `LexicalPath::is_discard`: `/dev/null`, or on Windows exactly the
  device-namespace spelling `\\.\NUL` (either slash, any case), and no other
  path. Every other Windows path whose last component is a DOS reserved device
  name — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`, any case,
  any extension, trailing dots and blanks trimmed — is *refused*, not
  excused: `lex::reserved_device_refusal` phrases the error (for `NUL`, as the
  question "did you mean `\\.\NUL`?"), and `capability::check_device_name`
  asks it at the three fs doors, `check_fs_op`, `Shell::locate` and
  `Shell::locate_existing`, ahead of any grant. A resolved path keeps its last
  component, so the check needs no raw spelling. Like the identity rules above
  both take `windows` as a parameter rather than reading `cfg!`, so neither
  table is dark on the other host.

`resolver.rs` composes the stages — `Resolver::resolve` is the *sole*
constructor of a `LexicalPath` (`forms.rs`, with its grant-side twin
`FrozenPath`), so canonicalisation cannot run before
sigil-expansion-then-lex: the ordering is in the types, not convention.

(`ral_path.rs` in the same directory owns `RAL_PATH` module search, used by `use`
and the plugin loader, not by grant matching.)

`which.rs` is the one `PATH` walk. **Its anchor is a `SearchCwd`, minted only
from a named provenance — `Context::search_cwd`, `Resolver::search_cwd`,
`SearchCwd::of` for a front end already holding the shell's cwd, or
`SearchCwd::nowhere` — so no call site can choose "here" for itself**
([[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]]).

- `path_dirs` is the sole directory list behind `locate`, `commands_on_path` and
  `search`; it **drops empty `PATH` elements on every platform**, so a trailing
  `;` or `:` never means the cwd. `.` and `./bin` are honoured, as written.
- `search` returns `PathSearch::{Executable, FoundNotExecutable, Missing}` from
  one traversal — the executable half memoised, the presence half not — so
  `runtime::command::vet` reads a verdict rather than taking a second walk.
- On Windows, `%PATHEXT%` suffixes **append**: `build.ps1` yields
  `build.ps1`, `build.ps1.EXE`, …, never `build.exe`.
- `ExecRules::compile` walks the process's own `PATH` with
  `SearchCwd::nowhere`, so neither a scoped `PATH` nor a relative entry can
  redirect a bare key.

A `FrozenPath` (`forms.rs`) carries its `surface` form (lexical — what
the author wrote, kept for display) and its `real` form (symlinks
followed), both fixed by one disk consultation at the freeze door; a guest
prefix is minted by `from_guest` under the POSIX fold, and `refreeze`
re-resolves a prefix against a live `Resolver`, the bare root minted rather
than frozen.
[[invariants/grants-judge-objects|Every authority is judged on `real`]]:
`contains::<P>` for fs, `RealPath::frozen` (`real.rs`) for the exec table,
and `evicts` for composition — and no other door: `lex::path_within` and its string twin are `pub(super)`,
so the form-blind kernel does not leave `core/src/path/`, `surface_path` is
private to `forms.rs`, and outside the module the surface leaves the type
only as a *string* (`as_str`, `into_string`) for rendering. That is enforced
rather than documented because the `xdg:` freeze guard once chose the form for
itself — asking on the surface while the check it guarded matched the real
form — and read a symlink out of `HOME` as contained.

The set-level algebra over prefixes is not in `path/`: it is
`capability::table`, which names polarities and never identities, so every
spelling question stays here. `real.rs` holds `RealPath`, the exec subject,
ordered as the host identifies files and ranked by `identity_depth`.

XDG base directories resolve through one resolver, `basedir.rs`
(`XdgKind`, `resolve_xdg`): an absolute `XDG_*_HOME` override else the
home-joined Linux default on every platform, and `None` where there is neither
— a host fact answers with an `Option`, so each caller picks its own fallback
rather than inheriting a fabricated home (the Windows sandbox ledger takes the
temp dir, keeping itself out of the cwd). Both the `xdg:` grant sigil
(`sigil.rs`) and the binary's own config/data loaders (`config.rs`) defer to
it, so a grant and the rc/history/plugin paths can never name different
directories — [[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].

## OS sandbox — `core/src/sandbox/`

External commands inside a `grant` block run under an OS sandbox enforcing the
declared **filesystem and network** capabilities — and **exec**, wherever a
backend carries the allow-list into the kernel, so a grant whose only opinion
is `exec` engages the sandbox too (`sandbox::EXEC_ENFORCED`, the one statement
of that fact, read by `capability::sandbox::sandbox_projection`). Exec is
guarded in-process on every platform (`capability::check_exec`) before the
spawn; both Unix
backends additionally render the same rules into the kernel, catching the
re-execs the in-process guard never sees (`sh -c`, `find -exec`) — macOS as
Seatbelt `process-exec` forms in rule order, Linux as a Landlock `Execute`
ruleset the payload enters inside the bwrap envelope over admits the parent
opened in the host (`linux/landlock.rs`;
Landlock being allow-list only, a deny *inside* an allowed directory is
subtracted from it, entry by entry over the tree at launch). `carriers.rs` computes what the kernel
must admit beside them for a script to start: system-vouched `#!`
interpreters and macOS shims, by `trusted_real` — regular files only, judged
against the effective uid — never `env`'s target
([[decisions/261004_exec-carriers|exec-carriers]]). The AppContainer on
Windows has no path-exec filter, so there the in-process guard stands alone (the deny-by-default fs projection still bounds
which images a child can *read*, and so load, at all).

Inside a *guest* — a VM whose engine runs under `ral-daemon`, signalled by
`RAL_GUEST` — the per-command OS backend is not engaged at all: every spawn
is already confined by the [[map/core/io-process|spawn jail]] (a fresh
unprivileged uid and a per-exec cgroup), the daemon disables the
unprivileged user namespaces bwrap needs, and the guest has no network
device for `net` to govern; the in-process guards apply unchanged
(`docs/SPEC.md` §12.11).

- `boot(&role)` — startup: pins ral's own executable (`reexec::OWN`) and, on Linux, the bwrap
  envelope (`linux::pin_envelope`, walked on the process's own `PATH`
  before any shell exists), and on Windows runs the boot-time orphan sweep
  (`windows::session::boot_recover`) that deletes a crashed prior session's
  AppContainer profiles and restores the per-session ACEs of any legacy
  pre-capability ledger. A test binary is
  the same [[invariants/single-binary|multicall executable]] a confined child
  re-execs, so it must serve these flags from its own pre-`main` `#[ctor]` (it
  reaches `main` only through libtest). `serve_pre_main(&role, installers)`
  (`core/src/sandbox.rs`) is the one pre-`main` dispatch, over the role
  `core/src/invocation.rs`'s `classify` named — run by `ral`, exarch and every
  test `#[ctor]` alike, surfacing a served role's exit code so the caller can
  terminate. It serves a `Warrant` role first (`serve_warrant`), before any
  pin, so the trampoline pins and opens nothing before it is confined; the
  pipeline anchor and the pgid probe spawn nothing and run unpinned. Every
  other role — the engine, the `BundledTool` multicall, the shell — runs
  `boot` first, so an `--engine` process has bwrap pinned exactly as the
  shell does. Skip it and `OWN` stays unpinned, so the per-command
  launcher refuses to re-exec a binary it cannot vouch for.
- `reexec.rs` — `Pinned`, an executable pinned at boot: the one shape every
  `Command` the sandbox execs is built from, so nothing a session does to its
  environment can choose the file. Two are pinned: this executable
  (`OWN`), so a confined re-exec runs the same binary even under an
  on-disk swap, and on Linux the bwrap envelope (`linux::ENVELOPE`). Its
  methods are `arg0` (the on-disk path, `argv[0]` of every exec), `exec_path`
  (what `execve` is handed), on Linux `fd` (the pinned descriptor) and `names`
  (the inode's current name, `None` once unlinked), and on macOS `verify`;
  `reexec::own()` is ral's own pin as a
  `Result`, so a sandboxed launch refuses when ral could not pin itself. The `Pin`
  variants say where a swap is even askable: `Fd` on Linux (the retained
  descriptor, so `/proc/self/fd/N` resolves to the boot inode), `Stat` on
  macOS (a `(dev, ino)` snapshot re-checked before each spawn), and
  `Unguarded` on Windows, which has no parent-side self re-exec for a guard
  to protect. On Windows there is no child re-entry at all — confinement is
  the AppContainer token the *parent* attaches at `CreateProcessW`, so a child
  asking to confine itself (`--warrant`) is refused with 126, and the pinned
  self serves to grant the container read on the bundled-tool re-exec image.
  `Pinned::verify`, the parent-side swap guard, is macOS's alone, which execs
  the trampoline by name; Linux execs it from ral's pin lent at
  `Slot::Trampoline`, so there is no name to verify.
- `projection_enforceable` (`sandbox.rs`) — rejects an offline (`net: false`)
  projection on a backend with no kernel network enforcement, so an unenforceable
  request fails closed rather than running ignored.
- `confinement_unavailable` (`sandbox.rs`) — the one refusal for a confinement
  this host cannot establish, whether `projection_enforceable` saw it coming,
  there was no bwrap on `PATH` to pin at boot (`linux::envelope`, asked at the
  first launch that needs it), or the pinned envelope failed to spawn.
- `Launch::new` (`process/launch.rs`) — builds the unsandboxed external exec image; `build_launch` (`runtime/command/process.rs`) reaches it directly when no projection is active.
- `launch.rs` (`sandboxed_command`) — the per-command launcher. `build_launch`
  (`runtime/command/process.rs`) routes an external or bundled child through here
  whenever a projection is active and the process is not already confined,
  confining that *one* child, the `Admitted`'s program: a `Program::File`
  by the real path the guard judged, its spelling as `argv[0]`, or a
  `Program::Tool`. macOS
  and Linux share one trampoline: the pinned ral itself, whose whole argv is
  `ral --warrant` (`WARRANT_FLAG`). The confinement to enter and the program to
  run cross in a `Warrant` (`sandbox/warrant.rs`, its fields private) compiled in
  the parent. The confinement is the `Confinement` trait — `spell`, `parse`,
  `enter` — implemented on macOS by `Seatbelt(String)`, the profile
  `macos::build_profile` compiled, and on Linux by `Option<Landlocked>`, spelled
  `unconfined`, `landlock` or `landlock+refer`;
  the program is a `Run`, `File(path, args)` or `Tool(tool, args)`.
  `Warrant::confine` is the only way to the program: receive and verify the
  warrant, enter the confinement (`macos::apply_profile`, `Landlocked::enter`),
  close the handoff range, and yield a `Confined`, whose `run` `execve`s the
  host program or runs the bundled tool in-process (`run_bundled`, shared with
  the `--ral-bundled-tool` multicall). `serve_warrant` serves the child: a
  failure before the program starts exits 126, and an `execve` refusal takes its
  code from `SpawnFailure` (`From<&io::Error>`) — 126, or 127 for a missing
  program.
  The encoding is NUL-terminated fields behind a `ral-warrant/1` magic — a
  `file` is its real path, then its `argv[0]` spelling, then its argv; a `tool`
  its name, then its argv — canonical (decode re-encodes and compares) and at
  most 8 MiB. It rides fd 99,
  never argv: a sealed memfd on Linux, checked again after sealing, and on macOS
  a prefilled socketpair whose writer closes before the child exists; the child
  refuses an unsealed memfd or a non-socket. The fixed descriptor layout is
  defined once as `Slot` in `warrant.rs`: 98 bwrap `--args`, 99 the warrant,
  100 `--info-fd` (a socketpair, which unlike a pipe cannot be reopened through
  `/proc`), 101 the Landlock ruleset, 102 ral's pin, the trampoline bwrap
  execs, 103.. the seccomp programs (sealed
  memfds), 107.. one mount handle per bind, open-ended and last. One `Handoff` lifts every source above every target and a
  single `pre_exec` `dup2`s each home; the child sweeps everything from 98 up
  with `close_range` once confined, and `Landlocked::enter` consumes the ruleset. `bwrap_command` refuses to launch if
  bwrap's pin fd sits on one of these slots: the `dup2` would close it before
  the exec of `/proc/self/fd/<N>`, which would then run whatever landed there.
  macOS spawns the trampoline directly; Linux spawns it under `bwrap`
  (`bwrap_command`), giving the full shape **bwrap → ral trampoline
  → Landlock → `execve`**. The real argv is `bwrap --args 98 --
  /proc/self/fd/102 --warrant`: `bwrap_argv` is pure in descriptors and returns bwrap's options alone, which
  bwrap takes from `--args`. The order is forced rather than chosen: a Landlock
  domain handling any fs right forbids `mount(2)`, bwrap's first act, so the
  layer can only be entered *inside* the envelope bwrap has already built.
  Linux's confinement names no path: the parent opens every admit by handle,
  never through a symlink, so each rule reaches the file a grant froze.
  `landlock::build` plans the
  handled set from the parent's one probe (`plan`), creates the `Ruleset` by raw
  syscall, and admits ral's pin fd, the loader base, each allowed file as
  itself and each live allowed directory as its hierarchy less what the table
  blocks beneath it (`expand`, which stops where a bind mount loops back onto
  a directory it is listing), each opened in the host by `open_real`
  (`RESOLVE_NO_SYMLINKS`); an absent admit is dropped, a file admit whose path
  has since become a directory is dropped, not admitted as a hierarchy, a
  symlink since planted refuses the launch, and a restricting exec projection
  is refused where Landlock is unavailable. The payload probes nothing: it takes
  the ruleset at its slot, adds only `Refer` on its own root, which exists
  nowhere else, and fails closed if a promised ruleset never arrived.
  `bwrap_command` takes the `Envelope` (bwrap's pin, the trampoline's, and
  the `HostEnvelope` probed once by running that trampoline as a pipeline
  anchor), the image (`Program::File`'s real path, the file the trampoline
  execs in turn), the handoff so far and the rendered projection, and refuses
  outright under a bwrap that builds no envelope, in bwrap's own words, or one
  without `--ro-bind-fd` (`HostEnvelope::builds`). `Binds::open` is the one
  place a launch opens what it mounts: the read-only defaults, the read and
  write prefixes, the image, each allowed file and live allowed directory of
  `ExecRules::from_kernel`, and the cgroup tree in the `Over` layer, each object
  (`render_objects`: a path's canonical spelling, its other spellings mounted
  too unless within another bind) opened once by `open_real` and lent per
  bind at `Slot::Mount(i)`. A read-only bind within another bind is dropped, a
  writable one only within a writable one; an absent object drops its binds,
  and so does a system default this user may not reach, while a name now
  reaching outside what was rendered refuses the launch. `Frozen` binds lay
  each covered admit read-only wherever a writable bind shows it, and each
  deny mask goes on wherever a `Shown` bind shows the denied path
  (`Binds::names`): a mount laid inside one bind of an object does not show
  at another, so a prefix written through a symlink, two binds of one
  handle, is frozen and masked at both. A deny hiding `/proc` is refused,
  the envelope starting its trampoline through it. The two pins join the `Over` layer by copies of their own
  descriptors, read-only at the names their inodes hold now (`pinned`, over
  `Pinned::names`), an unlinked pin binding nothing. `bwrap_argv` emits
  `--ro-bind-fd`/`--bind-fd` from the same vector, `Shown`, then `Frozen`,
  before `/proc` and `/dev`, `Over` after. `--chdir` refuses
  non-UTF-8, and so does every rendered name, rather than go lossy into
  bwrap's argv. Windows builds the
  target's `Launch` directly and `windows::session::confine` attaches its
  projection's AppContainer LowBox `SECURITY_CAPABILITIES`, so the parent's own
  spawn is the confinement point — never a re-exec child; a bundled tool there
  is a plain `ral --ral-bundled-tool <tool>` carrying no warrant, and
  `serve_warrant` refuses `--warrant` outright. The launcher also takes an
  `Ownership` (`Kept` / `Surrendered`, the second variant `cfg(unix)` since only there does the verb that makes the
  distinction exist): it reaches the Linux backend alone, which decides the
  two ties between the session and the envelope — death (`--die-with-parent`)
  and address (`--info-fd`) — so a `detach`ed survivor keeps the birthing
  frame's projection, namespaces included, for life while dropping both
  ([[map/core/runtime|runtime]]). The grant body itself evaluates
  locally, external children being confined per-command
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`; `Profile`, a record whose
  field order is the rule precedence, and in every profile ral's own file is
  write-denied and its ancestors unlink-denied), `fork_brake.rs` (macOS's
  process budget: the confined child's `RLIMIT_NPROC`, the user's count at
  launch plus 512), `linux.rs` (bwrap: the
  `Pinned` envelope, exec'd by descriptor and never by name, its own file
  read-only bound inside every envelope after the projection's binds; the
  argv — `--new-session`, the ipc/uts/cgroup namespaces and, by host fact, the
  pid one, `--proc` on both projections, the seccomp deny-set
  (`sandbox/linux/seccomp.rs`: `Filter`, `Syscall`, `explain`); `InfoFd`, the
  payload's pid read back for a `Kept` launch; `linux/host.rs`, `HostEnvelope`,
  the probed host facts the render is pure in), and
  `windows.rs` (Job Objects capping the child tree at 512 processes, plus the
  AppContainer backend in three submodules — `appcontainer.rs`, the profile
  lifecycle and LowBox `SECURITY_CAPABILITIES` construction; `dacl.rs`, the
  path-derived capability-SID engine (`fs_capability_name`, `ensure_fs_grant`)
  over a durably persisted stamp store, per-path named mutexes, and boot-time
  orphan recovery; `session.rs`, the session state the two compose into). The
  module docs of those three files carry the full protocol; the shape is
  imitated from MXC's Tier-3 processcontainer backend, breadcrumbed per unit.

The Windows backend is **path-keyed**: each `(canonical path, kind)` grant —
kind ∈ {rw, ro, deny} — derives a deterministic capability name
`ral.fs.<kind>.<128-bit-truncated SHA-256 of the canonical path>` — hashed as-is,
never case-folded, so a case-sensitive directory's two distinct names cannot
merge into one authority — and thence a capability SID via
`DeriveCapabilitySidsFromName` (`dacl::fs_capability_name`).
`dacl::ensure_fs_grant` stamps that SID's
inheritable ACE once and never reverts it. **A read-write grant is two
permanent mutations, and neither witnesses the other**: the mandatory-integrity
check runs before the `AppContainer` pass, and an unlabeled object defaults to
`Medium`, refusing every `Low`-IL child whatever the DACL says — so
`ensure_low_integrity_label` also stamps a `Low` `SYSTEM_MANDATORY_LABEL_ACE`,
asked *before* the ACE's witness is consulted and witnessed under its own stamp
key and its own SACL probe. For each mutation two witnesses gate skipping it —
the grow-only stamp store (`stamps.json`, atomic tmp+rename, per-path
named-mutex merge) recording completed propagations, and a probe of the root's
own DACL confirming the tree was not deleted and recreated. Recording follows
the apply, so a crash mid-propagation leaves no witness and the next grant
re-stamps idempotently while a child in the interim fails closed. A spawn's
kernel-checked reach is then exactly the capability SIDs `session::confine`
mints into its token, so attenuation shrinks-only: a narrowed grant or subagent
gets a token lacking the wider paths' capabilities. An ACE lives on the NTFS
object and Windows does not re-inherit on a same-volume rename, so stamped
authority is object-sticky where a grant rule is path-based — `dacl.rs`'s module
header records the resulting drift in both directions
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
Within that shape: an AppContainer profile is still minted per *distinct fs
projection* (`SessionSandbox` maps `bind_spec` identity to profile) for the
deny-by-default token and named-object namespace separation, carrying no fs
authority, and `DaclManager` is the profile ledger — teardown deletes profiles
but restores no ACEs; a `deny_paths` entry is its own per-path deny capability
the token opts into, which canonical ACL ordering places ahead of any allow, so
projection-specific denies coexist on a shared path; the child's program image
is granted read-only so a user-installed binary or the bundled-tool self image
can load at all; and `net: false` is enforced by withholding the network
capability SIDs — a LowBox token without them cannot open a socket, so
`net_enforced()` holds on Windows.

`macos-base.sbpl` is the policy-independent Seatbelt base every rendered macOS
profile inherits, and `build_profile` lays the grant's rules over it. Both are
read rule by rule — what each admits, what it withholds and why, and what the
backend pays to express an object policy in Seatbelt's name language — in
[[internals/seatbelt-profile|seatbelt-profile]].

Path-scoped *exec* confinement on Linux is a Landlock layer the payload enters
inside the envelope; a deny inside an allowed directory is subtracted over
the tree at launch, and only a veto under a trusted admit stays with the
in-process guard —
[[decisions/260906_landlock-exec-layer|landlock-exec-layer]].

`diag.rs` (with per-platform readers in `diag/macos.rs` / `diag/linux.rs`) turns
a kernel-reported sandbox denial into an actionable hint on the
failing command's `Error`: it reads the kernel log over the call's wall window
(Seatbelt on macOS, the seccomp record inside bwrap on Linux), keeps only lines
attributable to the call's descendant PIDs, and appends them. **The operand's
class decides the remedy, and the hint answers once per class present**: a
`file-read*` path is offered to the grant's `read` set and a `file-write*` one
to `write` (advice that fails twice, otherwise), a `process-exec` path to the
`exec` set — the layer a re-exec through `sh -c` or `find -exec` reaches, and
the only one that sees it — a `network-*` denial to the `net:` bit alone, and a
`mach-*`/`ipc-*` operand to nothing at all, because a door is the base
profile's to decide and no grant widens one. `Denied` is that taxonomy as a
type, so a service name cannot arrive where a path is expected; withheld doors
carrying a reason (`macos.rs::withheld_doors`, the `.sbpl`'s prose as data) are
quoted with it and lead the hint, since they are the one class the reader
cannot act on — the securityd denial that broke `cargo fetch` was once ranked
below a `.GlobalPreferences.plist` probe and answered with a grant that would
have changed nothing. macOS logs fully-resolved paths, so the hint names the
exact path with the symlink caveat; on Linux, an operandless denial first tries
`describe_denial`, which consults
[[decisions/260906_seccomp-is-a-typed-deny-set|the seccomp deny-set]] by
syscall number and, when it names one, repeats that rule's own reason instead
of guessing at an fs grant — a foreign-ABI record (an `arch=` mismatch) is
named as such rather than misread as an unlisted syscall; only where the
deny-set has nothing to say does the hint fall back, and it then says the
record names no operand rather than guessing at a set to widen. Windows has no
kernel denial log to scrape at
all, so its arm gates on the exit code alone: only an access-denied-shaped
exit (`ERROR_ACCESS_DENIED` / `STATUS_ACCESS_DENIED`) under an active sandbox
yields the fixed, pathless hint — never a fabricated path.

This boundary is what [[map/exarch|exarch]] reuses as its sandbox. Bundled
tools route through the *exec* chokepoint in-process; their **filesystem**
access has no in-process guard, so a bundled tool is never inlined — it is
spawned as a `ral --ral-bundled-tool` child and, under a restrictive grant,
floored by the OS profile of the per-command sandbox it runs in
([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]).
That single binary carrying both ral and its coreutils is part of why ral
is a [[invariants/single-binary|single-binary]]. `docs/SPEC.md` gives the
formal capability calculus.

Every `fs`/process constructor in this layer is a reviewed *syscall site*: the
workspace bans the raw constructors via clippy `disallowed_methods`, so each call
site carries an `#[allow(… reason = "[…]")]` classifying it as a surfaced
exec image (`Launch::new`), silent infrastructure (the self re-exec, the
pinned binaries' exec (`Pinned::command`), the `ps` denial sampler, the
boot-time binary pin, the stamp-store and profile-ledger lifecycle), or
test scaffolding. The site
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the sites are only declared and accounted, with `core/tests/syscall_sites.rs`
failing CI on any unaccounted constructor.
