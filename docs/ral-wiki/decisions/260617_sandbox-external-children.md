---
status: active
---

# Sandbox external children, not grant bodies

**`grant` remains the sandbox boundary by confining every external child, not by
re-execing the interpreter around the grant body.** A grant is a dynamic effect
scope: it folds the live stack, checks the effects ral performs, and renders a
child policy for effects an admitted process may perform after spawn. The ral
interpreter stays in the caller process.

The split is semantic, not just mechanical:

- **`grant` is a meet.** Nested grants compose by intersection over authority.
  That algebra belongs to the evaluator's dynamic context, not to a hidden process
  boundary.
- **A sandbox is a spawn boundary.** Seatbelt, bwrap, and AppContainer constrain a
  process tree. They should wrap the process whose behaviour is untrusted: the
  external command ral is about to run.
- **RAL-owned effects stay in process.** Structured filesystem primitives,
  redirects, editor access, shell-state mutation, and the top-level exec judgment
  are decided by `capability::check_*(&Context, …)` before the effect happens.
- **Child-owned effects are kernel-backed.** When an admitted external command is
  spawned under a restrictive fs grant, ral renders the current effective policy
  and enters the OS sandbox in that child before `execve`/process launch. A child
  that reads, writes, or re-execs behind ral's back is then held by the kernel
  layer where the backend can express it.

The endpoint-shaped `net` story leaves; an all-or-nothing **offline** child
sandbox axis may stay. ral has no network primitive for an in-process gate, so a
network-denying grant can only mean "spawned children get no network". That is
honest on Linux (`--unshare-net`) and macOS (omit `(allow network*)`), but a
Windows no-op is not acceptable: a backend that cannot enforce offline mode must
fail closed when that mode is requested.

On Windows the credible designs are:

- **AppContainer identity.** Launch the child in a per-profile AppContainer with
  no network capabilities, or use that AppContainer SID as the identity a WFP
  rule matches. This gives a per-spawn-ish principal; the cost is making ordinary
  desktop tools run under AppContainer filesystem/DLL restrictions and ACLing the
  scratch/allowed paths correctly.
- **WFP dynamic filters keyed to a unique child identity.** WFP can block at ALE
  connect/listen/resource-assignment layers, but image-path rules are too coarse:
  a `ral --ral-bundled-tool` child has the same image as the parent, and a `git`
  rule would hit unrelated `git` processes while installed. A filter is acceptable
  only if it keys on a unique package/AppContainer SID or a brokered per-child
  identity. A PID-only design needs a callout/driver, not plain static filters.
- **Fail closed.** Until one of those exists, offline mode on Windows is
  unsupported rather than advisory.

This keeps the language's founding promise: an exarch turn or ral script runs
under a grant frame whose admitted commands cannot freely read and write outside
the frame. What changes is the locus of the process boundary. The interpreter is
trusted policy machinery; external programs are the untrusted computations.

## Consequences

- Ordinary `grant { … }` evaluation is local again. Closures, handlers, audit
  ownership, cancellation, and shell state are not transported across IPC merely
  because a block mentions `fs`.
- `WireMobile`, `run_confined`, the authenticated confinement marker, and the
  parent-side sandbox IPC cancellation path stop being load-bearing for ordinary
  grant scope. They may survive only for an explicit whole-interpreter sandbox or
  other child-eval consumers.
- The in-process/OS split remains. Removing body re-exec does not collapse the two
  enforcers: the evaluator still judges ral-dispatched effects, and the kernel
  still confines a spawned program's own effects.
- Bundled commands must choose one side of the line. If they execute in-process,
  their filesystem effects go through `check_fs_op`; if they cannot do that
  cleanly, they run as an exec image and receive the same child sandbox as any
  other external.
- A whole-script sandbox is a separate construct. For untrusted ral source or
  defence in depth against interpreter/plugin bugs, add an explicit
  `sandbox { … }` / `ral --sandbox` boundary that evaluates a complete turn in a
  confined interpreter child and returns only a serialisable result. It is not the
  default meaning of `grant`.

## Safety invariant

**There must be no third path for filesystem effects.** Under an fs-restricting
grant, every operation that can touch the filesystem is either:

- *ral-owned* — evaluated in the interpreter and checked by
  `capability::check_fs_op(&Context, …)` on the canonical path before the syscall;
- *child-owned* — launched as a process under the effective `SandboxProjection`;
- *outside the grant effect surface* — startup/profile machinery whose authority
  is deliberately not user-code authority.

The old body re-exec bought defence in depth by putting even unchecked
interpreter syscalls under Seatbelt/bwrap. This design removes that accidental
floor, so the replacement proof obligation is explicit: redirects, globbing,
module loading, temp-file creation, structured reads/writes, helper transports,
and bundled commands are audited into one of the three buckets above. A bundled
command that cannot route every filesystem access through `check_fs_op` must run
as an exec image and receive the same child sandbox as an external.

The acceptance test is negative: every filesystem primitive and bundled head has
a denied-`fs` regression that proves it fails before the syscall or inside the
child sandbox, not by convention.

## Bundled commands

Bundled coreutils/diffutils/ripgrep heads follow
[[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]].
The fd/cwd/env bug that pushed them toward helper re-exec is real; the fix is to
make them ordinary command images, not to reintroduce grant-body re-exec.

- **Inline is ambient-only.** A bundled head may run in-process only in the clean
  terminal case: no active sandbox projection, no redirects or fd dups, no
  capture/audit tee, no pipeline staging, no env override, and no logical
  cwd/process cwd mismatch. The small uucore-global mutex protects only the
  upstream exit-code cell; it is not authority and it does not make fd/env/cwd
  mutation thread-local.
  This is the Windows cost valve: the common interactive `ls`/`cat`/`pwd` shape
  pays no process creation at all.
- **Every fs-restricting grant forces child placement.** Upstream uutils/ripgrep
  code opens paths internally and does not call ral's `check_fs_op`, so it is
  child-owned for grant purposes. The parent first checks exec authority, then
  launches `ral --ral-bundled-tool <tool> …` under the same effective
  `SandboxProjection` an external command would receive.
- **Redirects are child stdio, not parent fd surgery.** `>`, `2>`, `<`, and
  `2>&1` are realised by the ordinary child `Command` plumbing: owned files,
  pipes, inherited handles, and a child-local Unix `pre_exec` `dup2` when needed.
  The parent never temporarily rewires fd 0/1/2 to make an in-process library call
  look like an exec.
- **Pipeline byte stages are direct children.** A byte-only bundled stage is a
  process-stage image with normal stdin/stdout/stderr and, if needed, a start-gate
  fd/handle. It has no `StageJob::Uutils`, no `UutilsSnapshot`, and no
  `ChildEvalResponse`; value-edge bundled stages remain evaluator helper work.

## Pipeline helpers

The pipeline-stage re-exec stays. It is the principled remaining re-exec mode,
because a process-staged pipeline is already an OS process boundary, not a
lexical effect boundary pretending to be one.

- **Pure value pipelines do not spawn.** They remain the parent-side fold
  `x | f = f !{x}`.
- **A byte pipeline stage that is ral code is a subshell.** The helper owns the
  stage's stdin/stdout/stderr, process group / Job Object membership, foreground
  handoff, cancellation, and "changes do not flow back" state boundary. Those are
  process facts; evaluating the stage in the parent thread would reintroduce the
  same fd/cwd/env global-state hazards this decision is removing.
- **`child_eval` becomes narrower, not absent.** It should retain the
  `PipelineStage` runner and value-edge force/report path. It loses
  `ChildKind::Sandbox`, mobile-returning grant-body evaluation, authenticated
  sandbox markers, and the parent-side sandbox IPC watcher.
- **Grant semantics inside a helper are the same as in the parent.** The helper is
  trusted ral policy machinery carrying the grant stack. Its ral-owned filesystem
  effects are checked in-process; any external or bundled child it spawns is
  launched through the per-command sandbox path under the current effective fs
  projection.
- **Bundled byte stages do not use this helper.** They are direct
  `ExecImage::BundledTool` children unless they touch a value edge and therefore
  need evaluator semantics.

## Implementation deviation: direct byte stages are the NoTerminal case only

The "byte stages are direct children with a start-gate fd" letter above is
realised narrowly. `direct_spawnable` (`runtime/pipeline/resolve.rs`) admits the
`StageLaunch::Direct` path only when the stage carries no value edge **and** none
of the three conditions a bare direct child cannot serve hold: the pipeline does
not own the controlling terminal (`NoTerminal`), the stage has no redirects, and
no `!{…}` audit is capturing bytes. A byte stage that is foreground, redirected,
or byte-audited routes through the existing `HelperEval` re-exec instead of a new
direct-child-with-start-gate launcher. Those three conditions are exactly the ones
that need helper capabilities the helper already provides — park-on-stop and
`tcsetpgrp` handoff, redirect-as-child-stdio, and byte accounting — so reusing the
helper avoids building a third launch path that would reimplement them. This does
not weaken the safety invariant: the helper carries the grant stack and spawns its
external/bundled child under the same per-command `SandboxProjection` a direct
child would (`child_eval.rs`), and the denied-fs regressions cover the helper path
as a non-third fs path. The cost is one extra `ral` re-exec per affected
*host-external* stage; bundled stages pay nothing (their direct child is already a
`ral --ral-bundled-tool` process). On Windows the terminal plan is always
`NoTerminal`, so the foreground branch never applies there and the only residual
exposure is host-external stages with a redirect or under a byte-capturing audit.
The full direct-gate refactor remains available for exact letter fidelity if that
extra re-exec ever shows up as a latency cost (most plausibly on Windows).


## What disappears

The large deletion is not the OS backend renderers; those still define what a
child may do. The deletion is the machinery that made a lexical block pretend to
be a process:

- the evaluator branch that detects a restrictive grant body and swaps local
  evaluation for confined evaluation;
- sandbox IPC for ordinary grants: request/response framing, endpoint setup, and
  the one-body child server;
- the authenticated `RAL_SANDBOX_ACTIVE` marker as a condition for skipping nested
  grant confinement;
- `ChildKind::Sandbox` and the mobile-returning child-eval path used only to make
  body re-exec observationally local;
- parent-side cancellation watchers whose only job is to interrupt a child
  interpreter while the parent is blocked on synchronous sandbox IPC;
- endpoint-shaped network policy, the Windows advisory/no-op `net` path, and tests
  that treat network denial as more precise than offline child confinement.

What remains is smaller and more direct: a per-external command launcher that
takes the already-folded `SandboxProjection` and starts that command under the
platform sandbox.

## Implementation direction

- Delete the evaluator path that re-routes a restrictive grant body through
  sandbox IPC.
- At external dispatch, compute the effective grant once, perform the normal
  `check_exec_args`, then spawn the command through a platform sandbox launcher
  when the effective filesystem policy is restrictive or offline network mode is
  requested.
- Replace `net` with an explicit offline child-sandbox axis, or delete it if that
  narrower semantics is not accepted. No endpoint vocabulary, and no advisory
  backend: unsupported offline mode is an error.
- Re-scope `child_eval` to pipeline/helper execution and any future explicit
  whole-interpreter sandbox. It is no longer how `grant` acquires confinement.
- Keep `capability::check_*(&Context, …)` as the only authority-decision module;
  only the action taken after a successful external-spawn verdict changes.

## Implementation order

Do not delete grant-body confinement until per-command confinement exists. The
safe order is:

1. **Settle network semantics first.** Delete endpoint-shaped policy and the
   Windows advisory/no-op path. If network remains, it is only an offline
   child-sandbox axis (`deny network for spawned children`) and unsupported
   backends fail closed. If even that is rejected, remove the field entirely.
2. **Land bundled tools as exec images.** Introduce the resolved command image
   split (`Host` vs `BundledTool`), add the hidden bundled-tool entrypoint, route
   byte-only bundled pipeline stages through the direct child path, and delete
   `StageJob::Uutils` / `UutilsSnapshot`. Keep the clean-terminal inline fast path.
3. **Add per-command sandbox launching.** Make the ordinary command runner consume
   the current effective `SandboxProjection` and start `ExecImage::Host` and
   child-placed `ExecImage::BundledTool` under the platform backend. On Linux this
   is the bwrap command constructor; on macOS/Windows it may still be a tiny ral
   launcher process, but it launches one command, not a grant body.
4. **Flip `grant` body evaluation back to local.** Remove the transport branch
   that sends restrictive grant bodies through `run_confined`; keep
   `check_fs_op` and external dispatch tests red at first if any effect escaped
   the two-path invariant.
5. **Prune obsolete confinement IPC.** Delete `ChildKind::Sandbox`,
   mobile-returning sandbox child eval, authenticated sandbox markers,
   sandbox-request IPC, and the sandbox IPC cancellation watcher. Keep
   `ChildKind::PipelineStage` and the helper protocol.
6. **Audit denied-fs coverage.** Add or update negative tests for redirects,
   structured reads/writes, globbing, module/source loading, helper transports,
   external commands, bundled commands, and bundled byte pipelines. A failure must
   occur at `check_fs_op` or inside the child sandbox.


See also [[design/grant|grant]], [[design/two-enforcers|two-enforcers]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260610_child-eval-unification|child-eval-unification]], and
[[decisions/260617_sandbox-ipc-cancel|sandbox-ipc-cancel]].
