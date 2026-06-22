---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
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
  the editor/shell bool gates;
- `sandbox.rs` — the OS-renderable `sandbox_projection` builder;
- `exec.rs` — per-layer exec verdicts; the admitted arm carries `Admit`
  (`Any` / `Subcommands`), so a `Deny` cannot reach an allowed verdict;
- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a frozen `Capabilities`, one dimension
  decoder per `exec` / `fs` / `net` / `editor` / `shell` / `audit` key;
- `load.rs` — `load_capabilities_from_path` / `_from_str` for `.ral`
  capability profiles.

The capability *types* live in [[map/core/shell-state|types/capability]]: the
single always-frozen `Capabilities`, resolved at decode by the freeze pass
inside `decode_capability_map` ([[design/capability-freeze|freeze boundary]]);
plus `FsPolicy`, `GrantStack`,
`Meet`, `Join`, and the exec authority `ExecMap { literals, dirs }` — `literals`
keyed by name/path under the three-valued `ExecPolicy`, `dirs` keyed by
slash-free directory prefix under the two-valued `ExecDir`
([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]).

Path resolution for grant matching is `core/src/path/`: a fixed four-stage rule,
plus `which.rs` for PATH search.

- expand — `sigil.rs`, `tilde.rs`;
- lex — `lex.rs`;
- canonicalise — `canon.rs`;
- match — `lex::path_within`.

(`ral_path.rs` in the same directory owns `RAL_PATH` module search, used by `use`
and the plugin loader, not by grant matching.)

`prefix_set.rs` holds the `PrefixSet` algebra the sandbox projection folds with:
each prefix kept in both its *surface* form (lexical — what the OS profile emits,
since the sandbox matcher works lexically) and its *resolved* form (symlinks
followed — what intersection is judged on), so layers naming one directory through
different symlinks unify. The runtime, `Resolver`-backed counterpart to the lexical
`intersect_prefix_strings` behind `Capabilities::meet`. The duality is load-bearing,
not redundant: enforce the ceiling on the resolved form, emit the surface form the
sandboxed body will actually open.

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
Linux has no path-exec filter so there the in-process gate stands alone.

- `early_init(argv)` — startup: consumes `--sandbox-projection`, pins
  `SANDBOX_SELF`, and on Unix enters the OS sandbox for a per-command
  `--sandbox-projection` child (`maybe_enter_process_sandbox`). A test binary is
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
  confined re-exec runs the same binary even under an on-disk swap, with a
  per-platform identity check (`/proc/self/fd` on Linux, `(dev, ino)` snapshot on
  macOS, `BY_HANDLE_FILE_INFORMATION` on Windows). On Unix
  `maybe_enter_process_sandbox` enters the OS sandbox in a per-command
  `--sandbox-projection` child; on Windows it **fails closed** — a supplied
  policy it cannot enforce returns `Err`, never `Ok(None)` ("continue
  unconfined"). `verify_unswapped`, the parent-side swap guard, is
  `cfg(target_os = "macos")`: only macOS re-execs the *pinned self* parent-side
  (Linux re-execs through the fd, where a swap is already moot; Windows has no
  parent-side self re-exec).
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
  `serve_sandbox_exec` `execve`s the host target inside it; **Windows fails
  closed** — no per-command AppContainer / restricted-token backend exists, so a
  requested policy errors rather than running unsandboxed. The `--ral-sandbox-exec`
  sentinel and `serve_sandbox_exec`'s execve arm are `cfg(target_os = "macos")`,
  the only platform that emits the host re-exec tail. The grant body itself
  evaluates locally — `transport::dispatch` no longer re-execs it
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`), `linux.rs` (bwrap),
  `windows.rs` (Job Objects, capping the child tree at 512 processes) +
  `windows_restricted_token.rs` (a restricted token with every privilege dropped
  and integrity lowered to Low — the Chrome-renderer model: a file unreadable to
  the restricting SID set is unreadable to the child). The Windows backends supply
  resource caps and a profile dump; no path exists yet to confine a per-command
  child through them, so the entrypoint and launcher fail closed.

Path-scoped *exec* confinement is unenforced on Linux (no landlock backend) —
[[decisions/260530_linux-exec-confinement|linux-exec-confinement]].

`diag.rs` turns a kernel-reported sandbox denial into an actionable hint on the
failing command's `Error`: it reads the kernel log over the call's wall window
(Seatbelt on macOS, the seccomp record inside bwrap on Linux), keeps only lines
attributable to the call's descendant PIDs, and appends them. **Only a `file-*`
denial yields a concrete path to grant** — ipc/mach/network operands name a
service or endpoint, not a filesystem path, so they reproduce verbatim for
transparency but never fill the path-to-grant slot. macOS logs fully-resolved
paths, so the hint names the exact path with the symlink caveat; the Linux audit
record carries no path, so the hint degrades to "a sandboxed syscall was denied".

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
exec image (`make_command`), a silent infrastructure spawn (the self re-exec, the
`ps` denial sampler, the boot-time binary pin), or test scaffolding. The door
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the doors are only declared and accounted, with `core/tests/io_door_set.rs`
failing CI on any unaccounted constructor.
