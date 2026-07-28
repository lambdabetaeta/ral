---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
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
(`ctx.grants`): `admits_head`, `check_exec_args`, `check_fs_op`, the
editor/shell bool gates, and the OS-renderable `sandbox_projection`. The
`capability` module is the only place authority is decided — a module boundary
rather than a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). Why `Capabilities`,
the live judgment, and `SandboxProjection` are distinct and not one is argued in
[[design/capability-carriers|capability-carriers]].

Submodules:

- `enforce.rs` — the point-of-use gates: head admission, the
  audit-bearing exec/fs checks (`check_exec_args`, `check_fs_op`), and
  the editor/shell bool gates. The fs gate is split so the judgment is
  reusable without the report: `fs_verdict` is the pure decision, and
  `check_fs_op` is the layer that audits it and mints the `Break`;
- `sandbox.rs` — the OS-renderable `sandbox_projection` builder;
- `deputy.rs` — `deputy_prefixes`, the confused-deputy predicate: a
  prefix both `exec`-admitted and `fs`-writable, judged with `path::covers`
  on a *folded* `Capabilities` (neither layer of a meet is guilty alone).
  It reports rather than denies, and its findings surface at grant push and
  at an exarch profile load ([[design/grant|grant]]);
- `exec.rs` — per-layer exec verdicts; the admitted arm carries `Admit`
  (`Any` / `Subcommands`), so a `Deny` cannot reach an allowed verdict; the
  literal comparison is case- and PATHEXT-insensitive under Windows path
  semantics, so a bare `git` grant admits a resolved `GIT.EXE`;
- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a frozen `Capabilities`, one dimension
  decoder per `exec` / `fs` / `net` / `detach` / `editor` / `shell` / `audit`
  key; its
  exec-map freeze expands the two *exec-only* sigils `path:` (every `$PATH`
  component) and `system:` (the platform's tool roots,
  `sigil::system_tool_roots`), and drops bundled-tool grants for coreutils a
  host does not ship (`COREUTILS_UNIX_ONLY_TOOLS`);
- `load.rs` — `load_capabilities_from_path` / `_from_str` for `.ral`
  capability profiles.

The capability *types* live in [[map/core/shell-state|types/capability]]: the
single always-frozen `Capabilities`, resolved at decode by the freeze pass
inside `decode_capability_map` ([[design/capability-freeze|freeze boundary]]);
plus `FsPolicy`, `GrantStack`,
`Meet`, `Join`, and the exec authority
`ExecMap { literals, allow_dirs, deny_dirs }` — `literals` keyed by name/path
under the three-valued `ExecPolicy`, the two directory sets stored already
partitioned by verdict as `BTreeSet<NormalizedPrefix>`, so a meet folds the
partition it will use rather than re-deriving it, and a deny survives the
spelling and the depth it is judged on
([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]).

Path resolution for grant matching is `core/src/path/`: a fixed staged rule,
plus `which.rs` for PATH search.

- expand — `sigil.rs` (the five path-prefix sigils: `~`, `xdg:`, `cwd:`,
  `tempdir:`, `gitdir:` — the last three policy-only, expanded at freeze;
  `git.rs` backs `gitdir:` discovery), `tilde.rs` (`~user` resolves honestly
  per platform: off-Unix `get_user_home` declines rather than fabricating a
  home, and each call site picks its own fallback);
- lex — `lex.rs`;
- canonicalise — `canon.rs`;
- match — `lex::path_within`, which folds `starts_with_identity` over the
  alias pairs; under Windows path semantics that comparison unifies case,
  `/` vs `\`, and `\\?\`-verbatim spellings, so the fs-grant, exec-dir, and
  prefix-set matchers all inherit one notion of path identity.

`resolver.rs` composes the stages — `Resolver::resolve` is the *sole*
constructor of a `ResolvedPath` (`resolved.rs`, with its grant-side twin
`NormalizedPrefix`), so canonicalisation cannot run before
sigil-expansion-then-lex: the ordering is in the types, not convention.

(`ral_path.rs` in the same directory owns `RAL_PATH` module search, used by `use`
and the plugin loader, not by grant matching.)

A `NormalizedPrefix` (`resolved.rs`) carries its `surface` form (lexical —
what the OS profile emits, since the sandbox matcher works lexically), its
`resolved` form (symlinks followed — what containment and intersection are
judged on), and its `Namespace`, all fixed by one disk consultation at the
freeze door. The duality is load-bearing, not redundant: enforce the ceiling
on the resolved form, emit the surface form the sandboxed body will actually
open.

`prefix_set.rs` therefore contributes only the *set*-level algebra, pure and
disk-free: `covers` is the one containment judgment, keyed on
`(namespace, resolved)` so prefixes in different namespaces never overlap and
a cross-namespace meet is the empty, fail-closed intersection; `meet_prefixes`
is the kernel `PrefixSet::meet` and every `types::capability` lattice meet
share. `PrefixSet::resolve` is the lone door here that still holds a
`Resolver` — the sandbox-projection fold, which must render a prefix that was
never frozen (a bare exec-dir string, a `~`-headed fs prefix).

XDG base directories resolve through one resolver, `basedir.rs`
(`XdgKind`, `resolve_xdg`): an absolute `$XDG_*_HOME` override else the
home-joined Linux default on every platform. Both the `xdg:` grant sigil
(`sigil.rs`) and the binary's own config/data loaders (`config.rs`) defer to
it, so a grant and the rc/history/plugin paths can never name different
directories — [[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].

## OS sandbox — `core/src/sandbox/`

External commands inside a `grant` block run under an OS sandbox enforcing the
declared **filesystem and network** capabilities. Exec is gated in-process on
every platform (`capability::check_exec_args`) before the spawn; on macOS the
Seatbelt profile additionally renders a `process-exec` allow-list, catching the
re-execs the in-process check never sees (`sh -c`, `find -exec`), while bwrap on
Linux and the AppContainer on Windows have no path-exec filter so there the
in-process gate stands alone (on Windows the deny-by-default fs projection
still bounds which images a child can *read*, and so load, at all).

Inside a *guest* — a VM whose engine runs under `ral-daemon`, signalled by
`RAL_GUEST` — the per-command OS backend is not engaged at all: every spawn
is already confined by the [[map/core/io-process|spawn jail]] (a fresh
unprivileged uid and a per-exec cgroup), the daemon disables the
unprivileged user namespaces bwrap needs, and the guest has no network
device for `net` to govern; the in-process gates apply unchanged
(`docs/SPEC.md` §11.9, §15.2).

- `early_init(argv)` — startup: consumes `--sandbox-projection`, pins
  `SANDBOX_SELF`, on Unix enters the OS sandbox for a per-command
  `--sandbox-projection` child (`maybe_enter_process_sandbox`), and on Windows
  runs the boot-time orphan sweep (`windows::session::boot_recover`) that
  reclaims DACL grants a crashed prior session left stamped. A test binary is
  the same [[invariants/single-binary|multicall executable]] a confined child
  re-execs, so it must serve these flags from its own pre-`main` `#[ctor]` (it
  reaches `main` only through libtest); `serve_sandbox_early_init` is the shared
  `Option<u8>` building block the pre-`main` dispatch uses for that — run by
  `main` and every test `#[ctor]` alike, surfacing the re-exec child's exit code
  so the caller can terminate, then serving the per-command re-exec tails
  (`serve_sandbox_exec` for a host external, `try_run_bundled_tool` for a bundled
  tool). Skip it and `SANDBOX_SELF` stays unpinned, so the per-command launcher
  cannot pin the binary it re-execs.
- `reexec.rs` — pins an immutable handle on this executable at boot so a
  confined re-exec runs the same binary even under an on-disk swap. The `Pin`
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
- `make_command` — wraps an external command in the active policy.
- `launch.rs` (`sandboxed_command`) — the per-command launcher. `build_command`
  (`runtime/command/process.rs`) routes an external or bundled child through here
  whenever a projection is active and the process is not already confined,
  confining that *one* child: a `LaunchTarget::Host` external, or a
  `LaunchTarget::BundledTool` placed as `ral --ral-bundled-tool <tool>`. Linux
  wraps each child in `bwrap` (`make_command_with_policy`); macOS re-execs the
  pinned self (`ral --sandbox-projection <json> --ral-sandbox-exec <host>`, or
  `--ral-bundled-tool <tool>`) so the child enters Seatbelt in `early_init`, then
  `serve_sandbox_exec` `execve`s the host target inside it; Windows builds the
  target's `Launch` directly and `windows::session::confine` attaches its
  projection's AppContainer LowBox `SECURITY_CAPABILITIES`, so the parent's own
  spawn is the confinement point — never a re-exec child. The
  `--ral-sandbox-exec` sentinel and `serve_sandbox_exec`'s execve arm are
  `cfg(target_os = "macos")`, the only platform that emits the host re-exec
  tail. The launcher also takes an `Ownership` (`Kept` / `Surrendered`, the
  second variant `cfg(unix)` since only there does the verb that makes the
  distinction exist): it reaches the Linux backend alone, which is the one
  that builds an envelope process to tie the child to us, so a `detach`ed
  survivor keeps the birthing frame's projection for life while dropping
  that tie ([[map/core/runtime|runtime]]). The grant body itself evaluates
  locally, external children being confined per-command
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`), `linux.rs` (bwrap), and
  `windows.rs` (Job Objects capping the child tree at 512 processes, plus the
  AppContainer backend in three submodules — `appcontainer.rs`, the profile
  lifecycle and LowBox `SECURITY_CAPABILITIES` construction; `dacl.rs`,
  the crash-safe grant/deny ACE apply/restore engine with a durably persisted
  ledger, per-path named mutexes, and boot-time orphan recovery; `session.rs`,
  the projection-keyed session state the two compose into). The module docs of
  those three files carry the full protocol; the shape is imitated from MXC's
  Tier-3 processcontainer backend, breadcrumbed per unit
  ([[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]]).

The Windows backend is **projection-keyed**: one `DaclManager` per shell
session, one AppContainer profile per *distinct fs projection* (`SessionSandbox`
maps `bind_spec` identity to profile), so the ACEs behind a SID are exactly its
own projection's paths and a confined child's kernel-checked authority is the
projection its command declared — a narrowed grant or subagent gets a narrower
SID. Stamps persist until `teardown_session` (a detached worker keeps its
birth authority; a SID with no live child is inert); profile registrations are
ledgered before the OS create so the boot sweep deletes a crashed session's
profiles ([[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]]).
Within that shape: `deny_paths` nested inside a grant are stamped
unconditionally as explicit deny-ACEs, which canonical ACL ordering places
ahead of any allow; the child's program image is granted read-only so a
user-installed binary or the bundled-tool self image can load at all; and
`net: false` is enforced by withholding the network capability SIDs — a LowBox
token without them cannot open a socket, so `net_enforced()` holds on Windows.

`macos-base.sbpl` is the policy-independent Seatbelt base every rendered macOS
profile inherits: deny-default, libSystem/dyld startup allowances, common device
writes, and the runtime support needed before the policy-derived fs/net/exec rules
can matter. Its broad `file-ioctl` compatibility allowance deliberately leaves
`/dev/tty` configurable, because sandboxed full-screen children need termios and
window-size ioctls when a run has an explicit terminal loan. This is a known
hole: the current `SandboxProjection` carries fs/net/exec, not terminal-loan
state, so the Seatbelt profile cannot yet deny `/dev/tty` ioctls for ordinary
Denied-terminal tool runs while admitting them for `_ed-tui`-style children.
The notification-center carve-out is named in the POSIX shared-memory namespace
(`ipc-posix-name "apple.shm.notification_center"`), matching Apple's profiles.

Path-scoped *exec* confinement is unenforced on Linux (no landlock backend) —
[[decisions/260530_linux-exec-confinement|linux-exec-confinement]].

`diag.rs` (with per-platform readers in `diag/macos.rs` / `diag/linux.rs`) turns
a kernel-reported sandbox denial into an actionable hint on the
failing command's `Error`: it reads the kernel log over the call's wall window
(Seatbelt on macOS, the seccomp record inside bwrap on Linux), keeps only lines
attributable to the call's descendant PIDs, and appends them. **Only a `file-*`
denial yields a concrete path to grant** — ipc/mach/network operands name a
service or endpoint, not a filesystem path, so they reproduce verbatim for
transparency but never fill the path-to-grant slot. macOS logs fully-resolved
paths, so the hint names the exact path with the symlink caveat; the Linux audit
record carries no path, so the hint degrades to "a sandboxed syscall was denied".
Windows has no kernel denial log to scrape at all, so its arm gates on the exit
code alone: only an access-denied-shaped exit (`ERROR_ACCESS_DENIED` /
`STATUS_ACCESS_DENIED`) under an active sandbox yields the fixed, pathless hint —
never a fabricated path.

This boundary is what [[map/exarch|exarch]] reuses as its sandbox. Bundled
tools route through the *exec* chokepoint in-process; their **filesystem**
access has no in-process gate, so under a restrictive grant a bundled tool is
never inlined — it is spawned as a `ral --ral-bundled-tool` child and floored by
the OS profile of the per-command sandbox it runs in
([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]).
That single binary carrying both ral and its coreutils is part of why ral
is a [[invariants/single-binary|single-binary]]. `docs/SPEC.md` gives the
formal capability calculus.

Every `fs`/process constructor in this layer is a closed *I/O door*: the
workspace bans the raw constructors via clippy `disallowed_methods`, so each call
site carries an `#[allow(… reason = "[io-door:…]")]` classifying it as a surfaced
exec image (`make_command`), silent infrastructure (the self re-exec, the
`ps` denial sampler, the boot-time binary pin, the DACL ledger lifecycle), or
test scaffolding. The door
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the doors are only declared and accounted, with `core/tests/io_door_set.rs`
failing CI on any unaccounted constructor.
