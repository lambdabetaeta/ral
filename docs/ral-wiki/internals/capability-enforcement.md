---
verified_at_commit: 74e7a46
verified_at_date: 2026-07-30
anchors: [check_exec_args, check_fs_op, sandbox_projection, evaluate_exec, allow_region, deny_region, admitted_literal_paths, GrantStack, sandboxed_command, build_command, projection_enforceable, maybe_enter_process_sandbox, SessionSandbox, fs_capability_name, ensure_fs_grant]
---

# Capability enforcement: one chokepoint, two enforcers

[[design/grant|The grant design]] states the lattice — authority attenuated by
intersection. This is how a check actually runs: an in-process decision layer and
an OS sandbox that backs it for external commands, each authoritative exactly
where the other is blind ([[design/two-enforcers|two enforcers]];
`core/src/capability/`, `core/src/sandbox/`).

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
  never sees (`sh -c`, `find -exec`); bwrap on Linux and the AppContainer on
  Windows have no path-exec filter, so there the in-process gate stands alone
  ([[decisions/260530_linux-exec-confinement|linux-exec-confinement]]). That
  allow-list derives its admits from the same `evaluate_exec` verdict, per
  nameable command, so it never denies a command the in-process gate admits nor
  admits one it denies — a CI-enforced conservatism invariant
  ([[decisions/260704_exec-projection-defers-to-gate|exec-projection-defers-to-gate]]).
- *Filesystem* — gated in-process too (`check_fs_op`, read and write), and
  backed by an OS sandbox that confines a spawned child's own reads and writes:
  Seatbelt on macOS, bwrap on Linux, an AppContainer LowBox token on Windows.
  Gate and profile read *one* fold (`capability/fs.rs`: `allow_region` meets a
  region across layers, `deny_region` unions it), so here the conservatism
  invariant needs no differential test — the two cannot disagree. All that
  separates them is when the fold runs: afresh on every check for the gate,
  once at spawn for the profile, because that is when the profile is written.
- *Network* — no in-process gate at all, since ral dispatches no network
  operation itself, so the OS sandbox is the sole enforcer; on Windows the
  enforcement is the withheld network capability SIDs — a LowBox token
  without them cannot open a socket.

**A `deny` must survive the filesystem moving around it, not just name a path
to refuse.** [[design/grant|grant]] states the invariant: no confined child can
cause a denied path's contents to become reachable under a name the deny does
not cover. Writing, unlinking, or hard-linking a `deny_paths` entry itself is
already blocked — the Seatbelt profile renders a `subpath` deny for each — but
an *ancestor* of that entry sits outside its subpath and inside the write
prefix's own allow, so a confined `mv` or `rm` could relocate the ancestor
directory and carry the denied bytes to a name nothing covers.
`SandboxBindSpec::pinned_dirs` (`core/src/types/capability.rs`) closes the
gap: every proper ancestor of a `deny_paths` entry that lies within some write
prefix — the write prefix root included — is collected, over both a deny's
surface spelling and its symlink-resolved target, so a symlink swapped in
after sandbox entry is covered too. `build_profile`
(`core/src/sandbox/macos.rs`) emits `(deny file-write-unlink (literal
"<dir>"))` for each pinned directory, after the write prefix's own covering
allow (Seatbelt is last-match-wins); `literal`, never `subpath`, is what keeps
a pinned directory's *entries* mutable — only its own name-in-parent is
frozen. The price lands on macOS specifically: a grant that writes a repo and
denies `.git/config` also refuses `mv .git .git.bak` and `rmdir .git`, since
both `.git` and the repo root are pinned ancestors of the denied entry.

**Linux renders no pin, and that is not an oversight.** bwrap realizes a
`deny` as a mount laid over the denied path (`DenyMask`,
`core/src/sandbox/linux.rs`); a mount is anchored to the inode it covers, not
to the path string that named it at mount time, so renaming a non-mountpoint
ancestor carries the mask along and it keeps covering the real file at its new
location, while renaming or removing the mountpoint itself fails with `EBUSY`.
The invariant already holds by construction, so Linux pays a narrower price
than macOS: only the denied path's own name is frozen, and every ancestor —
the write prefix root included — stays freely renameable and removable.

**The shape of the denied path forces which mount, and the cost of confusing
them is the launch.** `--tmpfs` mkdirs its own mountpoint, so over an existing
regular file it dies with `ENOTDIR` before bwrap execs anything — a deny that
denies nothing because nothing runs. Hence `DenyMask::over` as the only
constructor: an existing non-directory takes `--ro-bind /dev/null`, bound
without `MS_DEV` and so unopenable either way; a directory or an absent name
takes `--perms 0000 --tmpfs`, absent names included because a mask must occupy
the name before the body runs or a child creates the file itself. Nothing
mounts over a symlink, so a symlinked `deny` is masked at the resolved target
`sandbox_projection` carries beside the surface spelling. Both masks refuse
with `EACCES` against macOS's `EPERM`, so a cross-platform test should assert
the bytes are unreachable rather than an errno — and because the failure mode
is a sandbox that never launches, one test per backend must spawn the envelope
for real
(`sandbox::linux::tests::a_denied_path_refuses_every_access_while_the_body_still_runs`).

**The sandbox is applied per external command, not by re-execing the grant
body.** A `grant` is a *local* dynamic effect scope: its body evaluates in
process, and `transport::dispatch` just runs that body locally — nested grants
compose by intersecting authority on the evaluator's `GrantStack`, which is not a
process boundary ([[design/grant|grant]]). Confinement happens one level down, at
external dispatch. When `build_command` (`runtime/command/process.rs`) spawns an
admitted external or bundled child under a restrictive projection, it routes
through `sandboxed_command` (`sandbox/launch.rs`), which confines that *one*
child:

- *Linux* wraps each child in `bwrap` via `make_command_with_policy`, threading
  the logical cwd in as `--chdir`;
- *macOS* re-execs a tiny launcher — `ral --sandbox-projection <json>
  --ral-sandbox-exec <host>` for a host external, or `--ral-bundled-tool <tool>`
  for a bundled tool — that enters Seatbelt in `early_init`
  (`maybe_enter_process_sandbox`) and then runs the one target inside it;
- *Windows* attaches the projection's AppContainer LowBox
  `SECURITY_CAPABILITIES` to the child's own `CreateProcessW`
  (`windows::session::confine`), so the parent's spawn is the confinement
  point — no re-exec child.

On Windows filesystem authority is *path*-keyed, and the token selects. Each
`(canonical path, kind)` grant derives a deterministic capability SID from a
hash of the canonical path; its ACE is stamped once, ever, and never reverted,
and `session::confine` mints into the child's token exactly the capability SIDs
its projection names. The kernel-level check therefore enforces the same
projection the in-process gate judges — a narrowed grant or a subagent's
narrowed permissions hold at the OS layer, because the narrower token does not
carry the wider paths' capabilities. Persistence is safe because a capability
SID is evaluated only in the AppContainer pass of the access check, whose result
intersects the normal user pass: an ACE no live token names is inert and can
never widen a process's reach past the owning user's own. A detached worker
therefore keeps the authority it was born with by construction. The residual is
that an ACE lives on the NTFS object while a grant rule names a path, and
Windows does not re-inherit on a same-volume rename: a file moved into a granted
tree stays dark, and one moved out of an rw tree keeps that tree's capability, so
path-based rules and object-sticky stamps agree only while the tree is still
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).

The launcher pins the *current binary* (`SANDBOX_SELF`, fixed at `early_init`) so
an on-disk swap cannot subvert it. Because confinement is per-command, the gate
fires only when a child is actually spawned: a `grant [net: false] { … }` with no
external child does not fail closed, and an offline request on a backend without
kernel network enforcement fails closed at the spawn (`projection_enforceable`).

The pipeline-stage helper re-exec is unchanged and unrelated: a process-staged
ral stage still runs through `run_child_eval` over one request/response frame
([[internals/pipeline-execution|pipeline execution]];
[[decisions/260610_child-eval-unification|child-eval-unification]]). That is a
real process boundary, not a lexical grant body pretending to be one.

**The hard rule for any such synchronous child wait: the host must own an
out-of-band cancellation path.** A parent blocked in a request/response frame
cannot observe its own foreground `CancelScope` by cooperative polling — the poll
never runs while the read is parked. Deadline and Esc therefore cannot break a
wedged frame unless the parent has a side channel that signals the confined child
subtree from outside the wait. Extra signal authority *inside* the child is not a
substitute: it lets a child signal its own descendants, but it does nothing to
free a parent stuck on the IPC edge. This is why the surviving `run_child_eval`
consumers keep teardown on the parent side rather than trusting the child to
notice cancellation.

A bundled coreutil's filesystem access has no in-process gate, so under a
restrictive grant it is never inlined: it is spawned as a `ral --ral-bundled-tool
<tool>` child that receives the same per-command sandbox as any external, which
is what floors it
([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]];
[[decisions/260617_sandbox-external-children|sandbox-external-children]]).

This is the boundary [[design/exarch-architecture|exarch]] reuses unchanged — an
agent run is a host-pushed grant frame over this same stack.

See also [[design/grant|grant]],
[[design/capability-carriers|capability-carriers]] (why the rule, the live
judgment, and the `SandboxProjection` are distinct, not one); map
[[map/core/capabilities|capabilities]]. `docs/SPEC.md` §11.
