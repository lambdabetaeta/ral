---
generated_at_commit: b1a0f280
generated_at_date: 2026-09-21
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
(`ctx.grants`): `admits_head`, `check_exec_args`, `check_fs_op` (a read by
name) and `check_fs_exact` (the object `Shell::locate` walked to), the
editor/shell bool gates, and the OS-renderable `sandbox_projection`. The
fold combines the layers' *verdicts*; there is no `Capabilities::meet`
([[decisions/260906_object-not-name|object-not-name]]). The
`capability` module is the only place authority is decided — a module boundary
rather than a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). Why `Capabilities`,
the live judgment, and `SandboxProjection` are distinct and not one is argued in
[[design/capability-carriers|capability-carriers]].

Submodules:

- `enforce.rs` — the point-of-use gates: head admission, the
  audit-bearing exec/fs checks (`check_exec_args`, `check_fs_op`,
  `check_fs_exact`), and the editor/shell bool gates. The fs gate is split
  so the judgment is reusable without the report: `fs_verdict` is the
  decision — `Guarded` first, for a write onto a boot-pinned sandbox binary
  (`sandbox::pinned_binary`, by inode), before any grant is folded —
  `check_fs_exact` audits it and mints the `Break` for a symlink-free path,
  and `check_fs_op` is the read-by-name layer over it that canonicalises
  leniently and excuses the discard device (`ResolvedPath::is_discard`)
  before either region is consulted. `Shell::locate` (`types/shell/checks.rs`)
  is the door every open takes: `path::walk` to the object, then
  `check_fs_exact` on `Located::real`; `GrantStack::admits_fs_exact` is the
  quiet twin a write card asks before reading a before-image;
- `sandbox.rs` — the OS-renderable `sandbox_projection` builder;
- `deputy.rs` — `deputy_prefixes`, the confused-deputy report: the prefixes a
  grant makes both `exec`-admitted and `fs`-writable, judged with
  `path::covers` on the `GrantStack`'s two prefix sets folded by
  `meet_prefixes` (neither layer is guilty alone). **What it locates is where a write becomes runnable, not an
  escalation**: within one projection the dropped binary is spawned under the
  confinement that wrote it, and only a runner outside the projection turns
  the shape into an escape — so it reports and never denies. Findings surface at grant push and at an exarch profile load
  ([[design/grant|grant]]);
- `exec.rs` — per-layer exec verdicts; the admitted arm carries `Admit`
  (`Any` / `Subcommands`), so a `Deny` cannot reach an allowed verdict; the
  literal comparison is case- and PATHEXT-insensitive under Windows path
  semantics, so a bare `git` grant admits a resolved `GIT.EXE`; every
  fold-equal key is met before the verdict, so a `git` deny still vetoes an
  exact `GIT` allow;
- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a frozen `Capabilities`, one dimension
  decoder per `exec` / `fs` / `net` / `detach` / `editor` / `shell` key —
  every one of them authority, none of them a recording switch
  ([[design/audit|audit]]); its
  exec-map freeze expands the two *exec-only* sigils `path:` (every `$PATH`
  component) and `system:` (the platform's tool roots,
  `sigil::system_tool_roots`), and drops bundled-tool grants for coreutils a
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
`Meet`, `Join`, and the exec authority
`ExecMap { literals, allow_dirs, deny_dirs }` — `literals` keyed by name/path
under the three-valued `ExecPolicy`, the two directory sets stored already
partitioned by verdict as `BTreeSet<NormalizedPrefix>`, so a verdict reads
the partition it needs rather than re-deriving it, and a deny survives the
spelling and the depth it is judged on
([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]).

Path resolution for grant matching is `core/src/path/`: a fixed staged rule,
plus `which.rs` for PATH search.

- expand — `sigil.rs` (the five path-prefix sigils: `~`, `xdg:`, `cwd:`,
  `tempdir:`, `gitdir:` — the last three policy-only, expanded at freeze;
  `git.rs` backs `gitdir:` discovery, following a `.git` pointer file only to a
  git directory whose `gitdir` back-pointer or `core.worktree` names the working
  tree back), `tilde.rs` (`~user` resolves honestly
  per platform: off-Unix `get_user_home` declines rather than fabricating a
  home, and each call site picks its own fallback);
- lex — `lex.rs`;
- canonicalise — `canon.rs`, for a read by name;
- locate — `walk.rs`, for an open: `walk` descends from the root through
  directory handles with no symlink followed by the kernel, splicing each
  link into the name and re-walking, and hands back a `Located` — the
  directory handle, the leaf, and the symlink-free `real` path — whose every
  operation is handle-relative with `FollowSymlinks::No` (`cap-primitives`
  supplies the `*at` calls on Unix and Windows);
- match — `lex::path_within`, the form-blind containment kernel, which folds
  `starts_with_identity` over the alias pairs; under Windows path semantics that comparison unifies case,
  `/` vs `\`, and `\\?\`-verbatim spellings, so the fs-grant, exec-dir, and
  prefix-set matchers all inherit one notion of path identity;
- name a device — `lex::is_discard_device`, behind
  `ResolvedPath::is_discard`: `/dev/null`, or on Windows a *last component*
  named `NUL` (the reserved name answers from any directory, in any case,
  behind an extension, and under `\\.\`). Like the identity rules above it
  takes `windows` as a parameter rather than reading `cfg!`, so neither
  table is dark on the other host.

`resolver.rs` composes the stages — `Resolver::resolve` is the *sole*
constructor of a `ResolvedPath` (`resolved.rs`, with its grant-side twin
`NormalizedPrefix`), so canonicalisation cannot run before
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
- `capability::sandbox::resolve_literal` anchors its exec-key resolution through
  `Resolver::search_cwd`, so the OS profile and the in-ral gate name the same
  binary.

A `NormalizedPrefix` (`resolved.rs`) carries its `surface` form (lexical — what
the author wrote, and what the OS profile emits, since the sandbox matcher works
lexically), its `resolved` form (symlinks followed), and its `Namespace`, all
fixed by one disk consultation at the freeze door. The duality is load-bearing,
not redundant, and **neither form is the real one**:
[[invariants/fs-judges-objects-exec-judges-names|fs authority is over objects
and is judged on `resolved`; exec authority is over names and is judged on
`surface`]]. So the type offers exactly two containment doors, one per
authority — `covers` (below) and `NormalizedPrefix::covers_name`, the exec
gate's — and no third: `lex::path_within` and its string twin are `pub(super)`,
so the form-blind kernel does not leave `core/src/path/`, `surface_path` is
private to `resolved.rs`, and outside the module the surface leaves the type
only as a *string* (`as_str`, `into_string`) for rendering. That is enforced
rather than documented because the `xdg:` freeze guard once chose the form for
itself — asking on the surface while the gate it guarded matched the resolved
form — and read a symlink out of `$HOME` as contained.

`prefix_set.rs` therefore contributes only the *set*-level algebra, pure and
disk-free: `covers` is the one *fs* containment judgment, keyed on
`(namespace, resolved)` so prefixes in different namespaces never overlap and
a cross-namespace meet is the empty, fail-closed intersection; `meet_prefixes`
is the kernel `PrefixSet::meet`, `ExecMap::join` and the deputy fold share;
`PrefixSet::outside` drops the allows a deny region covers, so no projection
carries an allow beneath a deny. `PrefixSet::resolve` is the lone door here that still holds a
`Resolver` — the sandbox-projection fold, which must render a prefix that was
never frozen (a bare exec-dir string, a `~`-headed fs prefix).

XDG base directories resolve through one resolver, `basedir.rs`
(`XdgKind`, `resolve_xdg`): an absolute `$XDG_*_HOME` override else the
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
gated in-process on every platform (`capability::check_exec_args`) before the
spawn; both Unix
backends additionally render the allow-list into the kernel, catching the
re-execs the in-process check never sees (`sh -c`, `find -exec`) — macOS as a
Seatbelt `process-exec` clause, Linux as a Landlock `Execute` ruleset the
payload enters inside the bwrap envelope (`linux/landlock.rs`; Landlock being
allow-list only, a deny *inside* an admit stays with the in-process gate
there). The AppContainer on Windows has no path-exec filter, so there the
in-process gate stands alone (the deny-by-default fs projection still bounds
which images a child can *read*, and so load, at all).

Inside a *guest* — a VM whose engine runs under `ral-daemon`, signalled by
`RAL_GUEST` — the per-command OS backend is not engaged at all: every spawn
is already confined by the [[map/core/io-process|spawn jail]] (a fresh
unprivileged uid and a per-exec cgroup), the daemon disables the
unprivileged user namespaces bwrap needs, and the guest has no network
device for `net` to govern; the in-process gates apply unchanged
(`docs/SPEC.md` §12.11).

- `early_init(argv)` — startup: consumes `--sandbox-projection`, pins
  `SANDBOX_SELF` and, on Linux, the bwrap envelope (`linux::register_envelope`,
  walked on the process's own `PATH` before any shell exists), on macOS enters
  the OS sandbox for a per-command
  `--sandbox-projection` child (`maybe_enter_process_sandbox`; Linux and
  Windows confine from the parent and refuse the flag), and on Windows
  runs the boot-time orphan sweep (`windows::session::boot_recover`) that
  deletes a crashed prior session's AppContainer profiles and restores the
  per-session ACEs of any legacy pre-capability ledger. A test binary is
  the same [[invariants/single-binary|multicall executable]] a confined child
  re-execs, so it must serve these flags from its own pre-`main` `#[ctor]` (it
  reaches `main` only through libtest); `serve_sandbox_early_init` is the shared
  `Option<u8>` building block the pre-`main` dispatch uses for that — run by
  `main` and every test `#[ctor]` alike, surfacing the re-exec child's exit code
  so the caller can terminate, then serving the per-command re-exec tails
  (`serve_sandbox_exec` for a host external, `try_run_bundled_tool` for a bundled
  tool). Skip it and `SANDBOX_SELF` stays unpinned, so the per-command launcher
  cannot pin the binary it re-execs.
- `reexec.rs` — `Pinned`, an executable pinned at boot: the one shape every
  `Command` the sandbox execs is built from, so nothing a session does to its
  environment can choose the file. Two are pinned: this executable
  (`SANDBOX_SELF`), so a confined re-exec runs the same binary even under an
  on-disk swap, and on Linux the bwrap envelope (`linux::ENVELOPE`). The `Pin`
  variants say where a swap is even askable: `Fd` on Linux (the retained
  descriptor, so `/proc/self/fd/N` resolves to the boot inode), `Stat` on
  macOS (a `(dev, ino)` snapshot re-checked before each spawn), and
  `Unguarded` on Windows, which has no parent-side self re-exec for a guard
  to protect. On Unix
  `maybe_enter_process_sandbox` enters the OS sandbox in a per-command
  `--sandbox-projection` child; on Windows there is no child re-entry at all —
  confinement is the AppContainer token the *parent* attaches at
  `CreateProcessW`, so a supplied `--sandbox-projection` is rejected as an
  error (no legitimate caller emits it), and the pinned self serves to grant
  the container read on the bundled-tool re-exec image. `verify_unswapped`,
  the parent-side swap guard, is `cfg(target_os = "macos")`: only macOS
  re-execs the *pinned self* parent-side (Linux re-execs through the fd, where
  a swap is already moot; Windows has no parent-side self re-exec).
- `projection_enforceable` (`sandbox.rs`) — rejects an offline (`net: false`)
  projection on a backend with no kernel network enforcement, so an unenforceable
  request fails closed rather than running ignored.
- `confinement_unavailable` (`sandbox.rs`) — the one refusal for a confinement
  this host cannot establish, whether `projection_enforceable` saw it coming,
  there was no bwrap on `PATH` to pin at boot (`linux::envelope`, asked at the
  first launch that needs it), or the pinned envelope failed to spawn.
- `Launch::new` (`process/launch.rs`) — builds the unsandboxed external exec image; `build_command` (`runtime/command/process.rs`) reaches it directly when no projection is active.
- `launch.rs` (`sandboxed_command`) — the per-command launcher. `build_command`
  (`runtime/command/process.rs`) routes an external or bundled child through here
  whenever a projection is active and the process is not already confined,
  confining that *one* child: a `LaunchTarget::Host` external, or a
  `LaunchTarget::BundledTool` placed as `ral --ral-bundled-tool <tool>`. macOS
  and Linux share one trampoline argv, `trampoline_tail`: the payload is the
  pinned ral itself, `ral --sandbox-projection <json>` followed by either
  `--ral-sandbox-exec <host>` or `--ral-bundled-tool <tool>`, so the child
  enters the process sandbox in `early_init` and only then becomes the target,
  `serve_sandbox_exec` `execve`ing a host program inside the confinement.
  macOS spawns that trampoline directly; Linux spawns it under `bwrap`
  (`make_command_with_policy`), giving the full shape **bwrap → ral trampoline
  → Landlock → `execve`**. That order is forced rather than chosen: a Landlock
  domain handling any fs right forbids `mount(2)`, bwrap's first act, so the
  layer can only be entered *inside* the envelope bwrap has already built.
  `make_command_with_policy` therefore takes a `Payload { program, args, image
  }` — `program` is the trampoline, `image` the host binary it will exec in
  turn — and binds both read-only where absolute, since bwrap cannot exec what
  it cannot see. Windows builds the
  target's `Launch` directly and `windows::session::confine` attaches its
  projection's AppContainer LowBox `SECURITY_CAPABILITIES`, so the parent's own
  spawn is the confinement point — never a re-exec child; the
  `--ral-sandbox-exec` sentinel and `serve_sandbox_exec`'s execve arm are
  `cfg(any(target_os = "linux", target_os = "macos"))`, Windows alone emitting
  no tail. The launcher also takes an `Ownership` (`Kept` / `Surrendered`, the
  second variant `cfg(unix)` since only there does the verb that makes the
  distinction exist): it reaches the Linux backend alone, which decides the
  two ties between the session and the envelope — death (`--die-with-parent`)
  and address (`--info-fd`) — so a `detach`ed survivor keeps the birthing
  frame's projection, namespaces included, for life while dropping both
  ([[map/core/runtime|runtime]]). The grant body itself evaluates
  locally, external children being confined per-command
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`), `linux.rs` (bwrap: the
  `Pinned` envelope, exec'd by descriptor and never by name, its own file
  read-only bound inside every envelope after the projection's binds; the
  argv — `--new-session`, the ipc/uts/cgroup namespaces and, by host fact, the
  pid one, `--proc` on both projections, the seccomp deny-set
  (`sandbox/linux/seccomp.rs`: `Filter`, `Syscall`, `explain`); `InfoFd`, the
  payload's pid read back for a `Kept` launch; `linux/host.rs`, `HostEnvelope`,
  the probed host facts the render is pure in, printed by the profile dump),
  and
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
inside the envelope; its deny sets stay with the in-process gate —
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
access has no in-process gate, so a bundled tool is never inlined — it is
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
exec image (`Launch::new`, `make_command_with_policy`), silent infrastructure (the self re-exec, the
`ps` denial sampler, the boot-time binary pin, the stamp-store and profile-ledger
lifecycle), or
test scaffolding. The site
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the sites are only declared and accounted, with `core/tests/syscall_sites.rs`
failing CI on any unaccounted constructor.
