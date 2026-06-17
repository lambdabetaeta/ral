---
verified_at_commit: d7e97288
verified_at_date: 2026-06-17
anchors: [check_exec_args, check_fs_op, sandbox_projection, GrantStack, run_confined, SandboxCancelWatch, marker_authenticated]
---

# Capability enforcement: one chokepoint, two enforcers

[[design/grant|The grant design]] states the lattice — authority attenuated by
intersection. This is how a check actually runs: an in-process decision layer and
an OS sandbox that backs it for external commands (`core/src/capability/`,
`core/src/sandbox/`).

**Every yes/no is a `capability::check_*(&Context, …)` that folds the whole
stack.** Each decision (`capability/enforce.rs`, `capability/sandbox.rs`) is a free
function over a borrowed `Context`, and each meets the dynamic `GrantStack`
(`ctx.grants`) before answering, so a verdict reflects authority intersected
across the *whole* stack, not a single frame:

- `check_exec_args`;
- `check_fs_op` (read / write);
- the editor/shell bool gates;
- `sandbox_projection`, the OS-renderable `SandboxProjection`.

The `capability` module is the only place authority is decided — a module
boundary, not a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). The fold composes the
layers by `Meet`:

- a dimension omitted from a grant inherits the ambient authority;
- a dimension present can only narrow;
- a deny is anti-monotonic — a later layer adds denies but never reopens a denied
  region ([[design/scoping|dynamic frames]]).

Exec is three-valued (Allow / Subcommands / Deny). The bundled coreutils and the
structured primitives route through this same chokepoint
([[internals/builtins-registry|builtins]]), which closes the bypass and lets ral
stay a [[invariants/single-binary|single binary]].

**Path matching is a fixed four-stage rule** (`core/src/path/`):

- expand sigils and `~`;
- lex;
- canonicalise (resolving symlinks);
- match by prefix (`path_within`).

Canonicalising *before* matching is why a directory scoped by a grant cannot be
escaped through a symlink or `..`.

**The in-process gate covers what ral dispatches; the OS sandbox covers what a
spawned process does on its own.**

- *Exec* — gated in-process on every platform: `check_exec_args` vets the
  arguments *before* the spawn. On macOS the Seatbelt profile additionally
  renders a `process-exec` allow-list, catching re-execs the in-process check
  never sees (`sh -c`, `find -exec`); bwrap on Linux has no path-exec filter, so
  there the in-process gate stands alone
  ([[decisions/260530_linux-exec-confinement|linux-exec-confinement]]).
- *Filesystem* — gated in-process too (`check_fs_op`, read and write), and
  backed by an OS sandbox that confines a spawned child's own reads and writes:
  Seatbelt on macOS, bwrap on Linux, AppContainer on Windows.
- *Network* — no in-process gate at all, since ral dispatches no network
  operation itself, so the OS sandbox is the sole enforcer.

**The sandbox is a re-exec of ral itself.** When an `fs`/`net`-restricting grant
engages, `run_confined` (`sandbox/runner.rs`) re-execs the *current binary* —
pinned at `early_init` so an on-disk swap cannot subvert it — carrying the
`SandboxProjection` as a CLI argument, and runs the body in that confined child
over an IPC channel (`sandbox/ipc/`). That channel ships one request frame in and
one response frame out — the *same* `child_eval` protocol a process-staged
pipeline stage uses ([[internals/pipeline-execution|pipeline execution]];
[[decisions/260610_child-eval-unification|child-eval-unification]]), so there is
one wire shape, not two that must agree. The re-exec is a transport detail: the
body returns its value, error, or escape to the caller exactly as an in-process
run would, plus an audit fragment. The OS sandbox itself is entered in
`early_init`, before the child's serve loop runs.
`confined_availability()` tells the
[[internals/evaluator-machine|evaluator]] whether to take the confined transport.

Because that IPC read is synchronous, cancellation of the enclosing foreground
scope needs a parent-side path. On Unix `SandboxCancelWatch` watches the same
scope while the parent is blocked in `run_confined`; `Interrupt` sends SIGINT to
the observed helper subtree, `Deadline` / `Explicit` send SIGTERM then SIGKILL,
and `RootAbort` sends SIGKILL immediately. The signals come from the
unconfined parent, not from inside Seatbelt/bwrap, so enforcing a host timeout
does not require widening child sandbox authority
([[decisions/260617_sandbox-ipc-cancel|sandbox-ipc-cancel]]).

**A process is trusted as already-confined only by an authenticated
marker, never the bare env var.** `RAL_SANDBOX_ACTIVE` is inheritable and
public-named, so its mere presence cannot mean "already inside the OS
sandbox" — an arbitrary parent could export it and switch the whole OS
layer off. The confined child instead adopts a per-re-exec capability
token its parent minted and shipped inside the IPC request frame (a
channel a wrapper cannot write to), recording it and stamping it into the
marker; `marker_authenticated()` trusts the marker only when its value
equals that token. The transport gate routes a restrictive grant body
local only when the marker authenticates, so a forged marker no longer
suppresses confinement, and a bundled coreutil's filesystem access —
which has no in-process gate — is floored by the OS profile of the
confined child it now necessarily runs in
([[decisions/260611_authenticated-confinement-marker|authenticated-confinement-marker]]).

This is the boundary [[design/exarch-architecture|exarch]] reuses unchanged — an
agent turn is a host-pushed grant frame over this same stack.

See also [[design/grant|grant]],
[[design/capability-carriers|capability-carriers]] (why the rule, the live
judgment, and the `SandboxProjection` are distinct, not one); map
[[map/core/capabilities|capabilities]]. `docs/SPEC.md` §11.
