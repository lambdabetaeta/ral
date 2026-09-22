---
generated_at_commit: d4af249c
generated_at_date: 2026-09-12
covers_paths: [core/src/runtime.rs, core/src/runtime/]
---

# Map: core / runtime

`core/src/runtime/` is the OS plumbing the CBPV [[map/core/evaluator|machine]]
dispatches into — command execution, pipeline orchestration, and the
per-child confinement choice. It re-enters evaluation only through
`evaluator::machine::apply` (a handler or alias arm's thunk, from
`command_call.rs`) and `evaluator::machine::evaluate` (a stage thread's own
closure, from `pipeline/thread.rs`) — stages carry closures, so the mutual
recursion is irreducible; the evaluator reaches it at
`PipeNode::launch`/`join` and, dispatching an `Exec` node, at
`command_call::classify_command` → `run_base_frame` / `run_external`, and the `command` redirect guards
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

- `command_call.rs` — `classify_command`, the single site that resolves a head
  (env → handlers → external) and runs the chosen arm; the evaluator's
  down-seam for a bare command. There is no builtin arm: a native table entry is
  an `Env` hit on a native value, and a base-frame row is a `Base` hit on the
  handler stack's base layer, run by `run_base_frame` with the argv slice
  ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
  [[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
  `^name` and a path-bearing head are the external directly, consulting
  neither the env nor the handler stack. **Grant admission is an
  external-command property**: only the `External` arm consults
  `capability::admits_head` before any argument evaluates, refusing the head
  outright; the env, base, and handler arms pass through. Handler and alias
  thunks are lambdas — a unary `{ |args| … }` or a catch-all `{ |name args| … }`
  — with the calling convention fixed by surface position, not inferred from a
  value's runtime shape
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
- `command.rs` — the External arm: vet the resolved identity, choose its
  *exec image*, wire stdio per the call's redirects, spawn, and reap.
  Submodules: `identity.rs` (`CommandIdentity`, the classify-once
  name/shown/resolved triple), `vet.rs` (existence → argv shape → grant policy,
  yielding a `SpawnPlan` with its `ExecImage`), `process.rs`, `child.rs`,
  `stdio.rs`, `redirect.rs`, `foreground.rs`, `detach.rs`, plus `uutils.rs`
  for [[map/core/builtins|bundled coreutils]].
  - **`resolved` and the 126/127 verdict are two projections of one `PATH`
    walk.** `identity::walk_path` calls `path::search` once, storing the
    `PathSearch` on the identity; `vet::check_existence` pattern-matches it and
    never probes the disk itself, so walk and verdict cannot disagree. Both that
    walk and `policy_names`' host-`PATH` baseline anchor through
    `Context::search_cwd` — the `within [dir: …]` override, else the `cd`-mutated
    cwd — so the grant gate judges the identity vet saw
    ([[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]]).
  - **The argv-shape step is one refused set read at two moments.**
    `vet::reject_exec_arg` maps each argument through `RefusedArg::of_value`
    (`core/src/types/exec_arg.rs`) and carries the shape's own `remedy`; the
    checker maps the argument's *type* through `RefusedArg::of_ty` before the
    run, so this is the backstop for what a type variable hid from it
    ([[invariants/exec-argv-is-words|exec-argv-is-words]],
    [[decisions/260812_exec-boundary-gated-statically|exec-boundary-gated-statically]]).
  - `detach.rs` is that same machinery — `CommandIdentity`, `vet`,
    `build_command` — up to the one act that differs: the child is born by
    `Launch::spawn_detached` ([[map/core/io-process|io-process]]), so its
    pgid is never observed here and nothing can signal, await, or reap it.
    What replaces the handle is a first-order `{pid, desc}` receipt. The
    birthing frame's projection is rendered into the launch exactly as for a
    child we keep, only with `sandbox::Ownership::Surrendered` dropping the
    parent-death tie, so the survivor's authority is frozen as that frame
    left it and no later frame can widen it — it cannot name the process at
    all ([[map/core/capabilities|capabilities]]). All three of its standard
    descriptors are `/dev/null`.
  - **A bundled coreutils/diffutils/ripgrep head is an `ExecImage::BundledTool`,
    always run as a `ral --ral-bundled-tool <tool>` child** — its inherited
    stdio, env, cwd, process group, and sandbox are the execution context, so it
    threads the same spawn/`RunningChild`/audit machinery as a host external,
    and behaves identically on Windows where no `.exe` exists to spawn
    ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
    [[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]).
  - `foreground.rs`'s `ForegroundDecision::for_standalone` gates the terminal
    handoff on a held *terminal lease* (`Shell::terminal_lease` is `Some`) plus
    top-level launch role and a terminal-bound sink with no shell-side pump, not
    on REPL interactivity — so a non-interactive script launched at a terminal
    holds a `Leased` run and foregrounds its interactive children, while an
    exarch `Denied` tool run cannot construct the handoff at all
    ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
    There is no parking: a stopped standalone child is answered with
    `SIGCONT` inline, by whoever waits on it, the same rule everywhere
    ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
  - Redirects install on the handler arm
    ([[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]).
    Child stdio routing — the `(Stdio, pump)` plan for a spawned child's
    stdout/stderr — is one shape, `Sink::child_stdout` / `child_stderr` yielding
    the shared `ChildStdioPlan` ([[map/core/io-process|io-process]]), through
    which the standalone command, a direct pipeline stage, and an external a
    stage thread spawns internally all route; the tty-inherit predicate
    (`stdio::inherit_tty`) is likewise shared.
  - The read door (`< file`), the write door (`> / >> / >|`, settled
    `committed`/`aborted`/`failed` at frame teardown), and the exec door (Host
    and `BundledTool` completion) each build an `Observation`
    (`core/src/types/observation.rs`) and pass it to `observe`
    (`core/src/evaluator/audit.rs`), the one fan-out door: it reports to the
    run's `Mooring` and to the open audit trail alike, judging neither's
    interest — the host filters the rail. It judges only whether anything
    happened: a write onto the discard device is dropped there
    ([[design/audit|audit]]). Core emits plain `Value::Map`s through
    `Observation::to_value`; a host (exarch) decodes their first-order wire
    form (`Observation::to_wire`) back with `Observation::from_wire`. The observation *shapes* and their card
    rendering live in [[map/exarch/io-surface|io-surface]]
    ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
- `pipeline/` — pipeline planning and execution. A multi-stage `CompKind::Pipeline`
  steps through a `PipeNode`: `PipeNode::launch` resolves the
  plan and spawns every stage into one process group, and `join`
  (`drive` then `fold`), called in the same rule, is what the arm
  meets; no frame is pushed, so an unwinding panic tears the group down
  through `PipelineGroup`'s `Drop` rather than through any undo of the
  machine's.  `group` (the pgid anchor and the foreground guard)
  stays alive across both `drive` and `fold` rather than dropped early
  ([[map/core/evaluator|evaluator]]). `PipeNode`'s field order is its teardown
  order: the
  collector drops before the group, so its kill still reaches a live pgid before
  the anchor is waited on. `resolve.rs` freezes each stage's launch decision once as
  `StageLaunch` (`Direct { id, args }` | `Thread`) from the head's resolution,
  its redirects, and whether a `!{…}` audit captures bytes, so launch reads a
  decision rather than re-deriving a dispatch gate. **No route enters that decision**: a
  stage's dispatch may not depend on where its payload lives, or the choice
  would stop being observationally transparent. **No route enters anywhere
  else, either**: the IR's own `PipeYield` — the pipeline's only
  value-transport question — never passes through resolve at all, riding on
  `PipeNode` and read once in the fold.
  `route.rs`'s `open_stage_routes` allocates every interior edge as an
  operating-system byte pipe from **stage position alone** — the zip of
  `Parent : ins` against `outs ++ [Parent]` — and no
  stage is told which one it is: every stage reports what it settled, and the
  fold, running in launch order, keeps the last `Ok`, which is the final
  stage's whenever that stage is `Ok`
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  A non-final stage's returned value is discarded, never serialised onto an
  edge; a `Thread` stage's final value rides home on its own
  `StageObservation`, no wire in between. A `Direct` stage is a process in the group; a `Thread`
  stage is an OS thread over a cloned `Shell`, and only an external it spawns
  — at any nesting depth — is a process. Ordinary application and bind do not
  enter this runtime.
  - A bundled (uutils) head routes `Direct` like any external, its
    `ral --ral-bundled-tool` child the image chosen by `command::build_command`
    — nothing in the pipeline distinguishes a bundled head from a host binary,
    so both classify as `External` carrying the resolved `CommandIdentity`.
    Value-style composition is evaluator application, never a stage-transport
    concern. The terminal-ownership decision (`resolve_terminal_plan`)
    likewise gates on a reachable terminal lease and a terminal-bound final
    sink, not on a `startup_foreground` predicate.
  - `pipeline.rs` is the spine, and `PipeNode::launch` *is* the launch loop:
    resolve, group, collector, node, then one `spawn_stage` per stage pushed
    onto the collector. The routes are a local declared *after* the node, so an
    error part-way through closes whatever the loop never consumed before the
    node tears down and a half-wired neighbour sees EOF. `launch.rs` wires one
    stage's byte endpoints (`route_parent_stdin`, `stage_stdin`,
    `wire_stage_stdio`)
    and dispatches it on its frozen `StageLaunch` (`spawn_stage`,
    `launch_external_stage_direct`); `stage.rs` holds
    `StageHandle`, which dispatches `cancel`/`cut`/`watch`/`arm` over
    its two kinds, `External`/`Thread` — `cancel` records the cause on its own
    `sent`, joined by `max`, and tells a thread's scope (a process hears a
    cancellation only as the signal the collector sends next); `cut` is that
    same cancel with `ReaderGone` plus the external's kill, `Effect::KillStage`'s
    action once the sentinel (`sentinel.rs`) has heard a dead write, never
    reused for another kill; and `watch` hands the collector a live
    external's wait handle for its own per-pid teardown — an `External` is a
    `crate::process::Watch` alone, the reaper's own subscription: no
    dedicated waiter thread, since `ChildHandle::into_watch`'s closure posts
    the exit straight onto the collector's channel as `Event::Ended(ix, _)`,
    and a stop never reaches the collector at all — the reaper answers it
    with `SIGCONT` before any subscriber sees one
    ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
    `StageEnd` carries a `Thread`'s already-built `StageObservation` or an
    `External`'s raw outcome, unsettled pumps, jail, and the `sent` its handle
    recorded; `StageEnd::settle` is what turns either into one
    `StageObservation`, in the fold and nowhere else. On an external's own
    `Event::Ended`, `StageHandle::file_external_end` reaps the watch — after
    the stage has left `stages`, so no later `KillStage` can ever name a
    reaped pid — and releases the held-open edge, leaving the jail and the
    pumps, which both wait on the group kill, to the fold;
    `group.rs`
    (`PipelineGroup::prepare` spawns the pgid anchor —
    `--ral-pipeline-anchor`, ignoring `SIGTSTP`/`SIGTTIN`/`SIGTTOU` outright
    and immune to termination signals — on every
    platform before any stage exists, and starts its witness in the same act:
    a report reader blocking to EOF on the anchor's stdout, posting
    `Event::Witnessed(cause)` per swallowed signal, and the anchor's own
    `ChildHandle` handed to the reaper as a `Watch` whose posting closure maps
    its exit to `Event::Cancelled`. Ownership is the anchor's presence, not a
    flag: `owned_pgid()` is `Some(leader)` exactly when the anchor is, and only
    that pgid may be signalled or killed; a joining group answers `None` and
    claims no foreground on its own account, while `holds_terminal`
    reports the handoff actually held rather than the plan it was launched
    under; `PipelineGroup::joining` is that no-anchor, no-pgid shape a
    nested pipeline inside a stage thread gets instead. The anchor's own rare
    stop is answered by the reaper like any other watched pid's, with no
    thread of the anchor's own involved. `AnchorProcess::finish` joins the
    report reader and drops the watch, whose `Drop` reaps. The group's verbs
    are lifecycle only — `prepare`/`joining`, `owned_pgid`, `holds_terminal`,
    `leader_pgid`, `claim_foreground`, `Drop`: signalling and killing belong to
    the collector, which is what holds the live stages.
    `CollectState::cancel_all(cause, delivered)` spells cancel, one
    grace signal per `Address` the collector enumerates — the pipeline's
    group or pids, unless `delivered`, and every confined stage's envelope
    group regardless — a bounded blocking `recv_timeout` grace, the
    kill (`kill_live`, the same addresses), a further
    blocking drain, while `CollectState::drop` kills whenever it is dropped
    with a stage still unobserved); `thread.rs` (`launch_thread_stage` wires a `Thread`
    stage's `Io` from its `StageRoute`, and its closure — given its
    `Slot { ix, tx }`, wrapped in a `SettleOnDrop` — builds its own
    `StageObservation` and sends `Event::Returned(ix, obs)` as its last act,
    the guard armed until then so an unwind reports in its place. A non-final
    stage's `stdout` and `ambient` are both its own edge; a **final** stage's
    are the parent's `stdout` and the parent's `ambient` respectively, so a
    discarded statement in the final stage leaves an enclosing capture rather
    than landing in its buffer ([[design/capture|capture]]).
    `ThreadStage` is the collector's handle onto the running thread: `cancel`
    cancels its scope and fires its wake — on Windows also retrying
    `CancelSynchronousIo` until the wake is acknowledged or the thread has
    finished, the one interrupt path there is — while
    `join_after_settled` reclaims a thread whose `Returned` has already
    arrived and `Drop` cancels-then-joins one never observed; a stage thread's
    own interior wait, for whatever child *it* spawned, answers a stop inline
    exactly as `RunningChild::wait` always does, structurally invisible to the
    collector); `collect.rs`
    (`CollectState` owns the
    stage handles from the first one launched, an `mpsc` channel every
    producer feeds — the reaper (through a stage's own `Watch`), a stage
    thread's own report, the sentinel, the anchor's witness, a `watch_cancel`
    on the mooring's scope — carrying
    `Event::{Ended, Returned, Wrote, Cancelled, Witnessed}`, with no
    `Stopped`/`Continued` variant at all, which the pure fold `step` folds
    over, returning at most one `Effect` (`Option<Effect>`) for a
    thin interpreter (`CollectState::run`) to perform:
    `ArmEdge`, `KillStage`, `CancelAll { cause, delivered }`. Completion is not
    among them — the pipeline is finished exactly when no stage handle is left
    (`live()`). `Witnessed` is what sets `delivered`: the kernel gave the whole
    pgid that signal already, so teardown sends no second copy of it.
    The `Slot` a stage hands its own producers is minted by `PipeNode::launch`,
    which holds the sending half as a local and drops it on return — the
    collector keeps none. Attribution rides the handle:
    `StageHandle::sent` is the strongest cause
    this collector ever sent that stage, joined by `max` and carried onto its
    `StageEnd`, and `StageEnd::settle`, called by `CollectState::fold` — the one
    place `&mut Shell` reaches an external's settlement (audit synthesis,
    exit-hint lookup, sandbox-denial augmentation, via
    `finish_external_settlement`) — is the one place that reads it back
    against what actually happened: a thread's forgiveness reads its own
    break alone (a `ReaderGone` break is only ever this collector's doing),
    an external's reads `sent == Some(ReaderGone)` and
    `outcome.is_stage_kill()` too. The verdicts join by `stronger`/`rank`, an
    escape outranking an error and ties going to the earlier stage;
    `drive`/`cancel_all`/`step` take no `&Shell` at all. `drive` is
    `while live() { recv; step; run }`, no interval and
    no backoff, returning when every stage is observed;
    `PipeNode::join` is `drive()`, then `fold(mooring, shell, yields)`, the
    group dropped after). `helper.rs` is the hidden
    `--ral-pipeline-anchor` / `--ral-bundled-tool` child entrypoints — the only
    two multicall flags here, a ral-written stage running on a thread of the
    parent process instead. On Windows every external a stage thread spawns still
    resolves `PgidPolicy::Join` against the anchor's registered Job Object,
    assigned at creation under the suspended create → assign → resume path
    ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).
- **A `grant` is a dynamic effect scope, not a process boundary, so the grant
  body always evaluates locally** — the machine steps its body in process, with
  no router in between:
  nested grants compose by intersection over authority, an algebra of the
  evaluator's dynamic context. Confinement happens elsewhere — the
  RAL-owned effects are decided in process by `capability::check_*`
  ([[map/core/capabilities|capabilities]]), and child-owned effects are
  kernel-backed at *external dispatch*: when a projection is active,
  `command::build_command` obtains a confined `process::Launch` via
  `projection_enforceable` / `sandboxed_command` — per-command Seatbelt /
  bwrap confinement on Unix; on Windows the LowBox token of the
  projection's own AppContainer profile, carrying the per-path fs
  capability SIDs its projection names
  ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]) —
  and net/fs fail-closed fires when a child is
  actually spawned, not at grant-body entry
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
  Inside a *guest*, `build_command` takes neither projection branch:
  `shell.guest_jail()` marks every spawn as already confined by the spawn
  jail — a fresh unprivileged uid and a per-exec cgroup,
  `process/jail.rs` ([[map/core/io-process|io-process]]) — since bwrap
  needs the user namespaces the guest boot disables; the in-process gates
  apply unchanged, and `child.rs` tracks the per-exec `JailCgroup`, so
  cancel and settle kill the whole tree through `cgroup.kill` (a
  `setsid`'d grandchild cannot leave its cgroup) while the grace phase
  stays pgid-addressed (`docs/SPEC.md` §12.11).
`core/src/engine_seed.rs` (crate root, beside the wire layer it rides, *not*
under `runtime/`) now carries only `EngineSeed`/`pack_seed`, the engine seat's
own seed wire for a wire-hatched child (`hatch.rs`); a pipeline stage no
longer crosses a wire at all and this module carries no pipeline-stage type
([[decisions/260902_stages-are-threads|stages-are-threads]],
[[decisions/260610_child-eval-unification|child-eval-unification]], superseded).

The `Shell` state these thread is [[map/core/shell-state|shell-state]]; the serde
mirror and wire envelope they ride is [[map/core/transport|transport]].
