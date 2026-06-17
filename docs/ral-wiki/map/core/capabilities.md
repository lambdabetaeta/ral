---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [core/src/capability/, core/src/capability.rs, core/src/sandbox/, core/src/sandbox.rs, core/src/path/, core/src/path.rs]
---

# Map: core / capabilities & sandbox

The [[design/grant|grant]] mechanism in two halves: an in-process decision layer
and an OS process sandbox that enforces it for external commands. Authority is
attenuated by intersection — a `grant` block can only narrow.

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
  `SANDBOX_SELF`, enters the OS sandbox (Unix), and dispatches the IPC child mode
  (`ipc::serve_from_env_fd` / `_handle`). A test binary is the same
  [[invariants/single-binary|multicall executable]] a confined child re-execs, so
  it must serve these flags from its own pre-`main` `#[ctor]` (it reaches `main`
  only through libtest); `early_init_or_exit_for_test_ctor` is that one shared
  wrapper, self-exiting when this run is the child. Skip it and `SANDBOX_SELF`
  stays unpinned, so `confined_availability()` reports `Unavailable` and the
  binary's confined-path tests cannot exercise the sandbox.
- `reexec.rs` — pins an immutable handle on this executable at boot so a
  confined re-exec runs the same binary even under an on-disk swap, with a
  per-platform identity check (`/proc/self/fd` on Linux, `(dev, ino)` snapshot on
  macOS, `BY_HANDLE_FILE_INFORMATION` on Windows).
- `marker.rs` — authenticates the `RAL_SANDBOX_ACTIVE` confinement marker
  against a per-re-exec capability token (`mint` / `adopt` /
  `authenticated`): a genuine child adopts the token its parent shipped
  in the IPC request, so a forged env var does not suppress confinement
  ([[decisions/260611_authenticated-confinement-marker|authenticated-confinement-marker]]).
- `make_command` — wraps an external command in the active policy.
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`), `linux.rs` (bwrap),
  `windows.rs` (Job Objects) + `windows_restricted_token.rs` (a restricted token
  with every privilege dropped and integrity lowered to Low — the Chrome-renderer
  model: a file unreadable to the restricting SID set is unreadable to the child).
- `runner.rs` (`run_confined`) re-execs ral inside the sandbox for confined
  evaluation and folds the response back (`fold_response`). The wire rides the
  shared [[map/core/runtime|child-eval runner]]: `ipc/` (`transport.rs` over a
  Unix socketpair, `transport_windows.rs` over a named pipe, `child.rs` for the
  child entry points) ships one `ChildEvalRequest` out and reads one
  `ChildEvalResponse` back (`crate::child_eval`), the same single-frame protocol
  the pipeline stage uses. The response carries the events the body surfaced in
  the child (`ChildEvalResponse.surface_events`, replayed through the parent's
  [[map/core/shell-state|surface sink]] once `run_confined` returns — batched,
  not live). The child runs `run_child_eval(.., ChildKind::Sandbox)` against a
  shell built through `subprocess::reexec_child_shell`, so its host builtins
  survive the re-exec. On Unix, `SandboxCancelWatch` watches the parent
  foreground `CancelScope` while the parent is blocked in IPC and signals the
  observed helper subtree from outside the OS sandbox on Esc/deadline/root abort
  ([[decisions/260617_sandbox-ipc-cancel|sandbox-ipc-cancel]]).
  `confined_availability()` tells the [[map/core/evaluator|evaluator]] whether
  to take the confined transport.

Path-scoped *exec* confinement is unenforced on Linux (no landlock backend) —
[[decisions/260530_linux-exec-confinement|linux-exec-confinement]].

This boundary is what [[map/exarch|exarch]] reuses as its sandbox. Bundled
tools route through the *exec* chokepoint in-process; their **filesystem**
access has no in-process gate and is floored only by the OS profile of the
confined child every `fs`/`net`-restricting grant body now runs in
([[decisions/260611_authenticated-confinement-marker|authenticated-confinement-marker]]).
That single binary carrying both ral and its coreutils is part of why ral
is a [[invariants/single-binary|single-binary]]. `docs/SPEC.md` gives the
formal capability calculus.
