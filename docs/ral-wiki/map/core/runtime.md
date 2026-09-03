---
generated_at_commit: 0c6ec335
generated_at_date: 2026-09-03
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

- `command_call.rs` — `run_call`, the single site that resolves a head
  (env → handlers → external) and runs the chosen arm; the evaluator's
  down-seam for a bare command. There is no builtin arm: a native table entry is
  an `Env` hit on a native value, and a base-frame row is a `Base` hit on the
  handler stack's base layer, run by `run_base_frame` with the argv slice
  ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
  [[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
  `^name` skips the env, and therefore every native, but still consults
  handlers; a path-bearing head skips handlers too. **Grant admission is an
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
    `Observation::to_value`; a host (exarch) decodes them back with
    `Observation::from_value`. The observation *shapes* and their card
    rendering live in [[map/exarch/io-surface|io-surface]]
    ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
- `pipeline/` — pipeline planning and execution. A multi-stage `CompKind::Pipeline`
  steps through a `PipeNode`: `PipeNode::launch` resolves the
  plan and spawns every stage into one process group, and `join`
  (`collect` then `finish`), called in the same rule, is what the arm
  meets; no frame is pushed, so an unwinding panic tears the group down
  through `PipelineGroup`'s `Drop` rather than through any undo of the
  machine's.  `group` (the pgid anchor, foreground guard, SIGINT relay)
  stays alive across both `collect` and `finish` rather than dropped early
  ([[map/core/evaluator|evaluator]]). `PipeNode`'s field order carries the same
  teardown invariant as `PipelineResources`'s: the
  collector drops before the group, so its kill still reaches a live pgid before
  the anchor is waited on. `resolve.rs` freezes each stage's launch decision once as
  `StageLaunch` (`Direct(ExternalStage)` | `Thread`) from the head's resolution, redirects,
  terminal ownership, and audit state, so launch reads a decision rather than
  re-deriving a dispatch gate. **No route enters that classification**: a
  stage's dispatch may not depend on where its payload lives, or the choice
  would stop being observationally transparent. **No route enters anywhere
  else, either**: the single fact resolve carries is `PipelinePlan::yields`,
  the IR's own `PipeYield`, which the checker wrote and the runtime only reads.
  `route.rs`'s `open_stage_routes` then allocates every interior edge as an
  operating-system byte pipe from **stage position alone** (`i + 1 < n`), and
  the collector derives `is_last` from `i + 1 == n` together with that one
  yield — the pipeline's only value-transport question
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  A non-final stage's returned value is discarded, never serialised onto an
  edge; a `Thread` stage's final value simply returns on its `JoinHandle`, no
  wire in between. A `Direct` stage is a process in the group; a `Thread`
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
  - `launch.rs` (`PipelineBuild` owns launch; a failed launch is its drop, the
    collector it carries killing the group before any stage handle joins;
    `StageHandle` dispatches `kill_now`/`kill_by_pid`/`cancel` over its two
    kinds, `External`/`Thread` — `kill_now` (the reader-gone cascade) is the
    one place permitted to raise the forgiven `Ending`; `kill_by_pid` is a
    joining collector's own teardown reaching what it launched directly, with
    no pgid of its own to kill — an `External` is an
    `ExternalWaiter`, the handle onto that stage's own dedicated waiter
    thread (`spawn_external_waiter`, wrapping
    `command::RunningChild::run_pipeline_stage`), which owns that child's
    wait exclusively and answers every stop that child sees with `SIGCONT`
    inline, invisible to the collector
    ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]); the
    collector never probes or `waitpid`s a pipeline
    stage at all, every settlement arriving as its own `Report::Settled`.
    `file_settled` is total over both kinds — an external's already-resolved
    observation just reclaims its waiter's join handle, a thread's is
    forgiven if its `Ending` says `ReaderGone` — carrying the one `Ending`
    every later decision about that stage reads); `group.rs`
    (`PipelineGroup::prepare` spawns the pgid anchor —
    `--ral-pipeline-anchor`, ignoring `SIGTSTP`/`SIGTTIN`/`SIGTTOU` outright
    and immune to termination signals — on every
    platform before any stage exists; `owned: bool` says whether this group
    spawned the anchor or joined an enclosing stage's — a joining group may
    not signal, kill, or claim the foreground on its own account — while
    `holds_terminal` reports the handoff actually held
    rather than the plan it was launched under; `PipelineGroup::joining` is the
    no-anchor, no-relay, may-not-signal shape a nested pipeline inside a stage
    thread gets instead. `start_witness_threads` (called once the collector's
    channel exists, since the anchor spawns before it does) starts the
    anchor's two dedicated threads — a report reader blocking to EOF on its
    stdout, an anchor waiter the sole reaper of its pid, answering its own
    rare stop with `SIGCONT` inline exactly as any other waiter does — both
    reporting only `Report::Witnessed(Cancelled(..))`, since a stop is never
    witnessed; `AnchorProcess::finish` joins them rather than
    reaping directly. The group's whole verb set is `signal` and `kill`, both
    `&self`; `CollectState::cancel_all` spells signal, a bounded blocking
    `recv_timeout` grace, kill, a further blocking drain, while
    `CollectState::drop` kills whenever it is dropped with a stage still
    unobserved); `thread.rs` (`launch_thread_stage` wires a `Thread`
    stage's `Io` from its `StageRoute`, and its closure — given its own index,
    finality, and a sender clone — builds its own `StageObservation` and
    sends `Report::Settled(ix, Settlement::Thread(obs))` as its last act, the
    sender bound in the closure's outermost frame so an unwind drops it too;
    `ThreadStage` is the collector's handle onto the running thread, kept
    only to join once its `Settled` event has arrived, or to recover a
    panic's message when it never does — a stage thread's own interior stop,
    a child *it* spawned stopping, is answered inline by that child's own
    `RunningChild::wait`, structurally invisible to the collector, so there
    is no interior-stop watcher left to hold a handle onto); `collect.rs`
    (`CollectState` owns the
    stage handles from the first one launched, an `mpsc` channel every
    producer thread feeds — stage threads, external waiters, the anchor's two
    threads, a low-frequency cancel-scope timer, the one left — resolved
    (`resolve`, the one place `&Shell` reaches an external's settlement) into
    the `Event`s the pure fold `step` folds over — `Settled`, `Witnessed`,
    `Cancelled`, with no `Stopped`/`Continued` event at all — returning the
    `Effect`s a thin interpreter (`run`) performs — `KillStage`, `CancelAll`,
    `Done`; `drive` is
    `loop { for e in step(&mut st, rx.recv()?) { run(e) } }`, no interval, no
    backoff, and its own `Drive` sum is just `Done`, since nothing parks
    any more). `helper.rs` is the hidden
    `--ral-pipeline-anchor` / `--ral-bundled-tool` child entrypoints — the only
    two multicall flags left here, since a ral-written stage no longer
    re-execs at all. On Windows every external a stage thread spawns still
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
