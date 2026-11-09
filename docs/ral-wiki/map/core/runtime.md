---
generated_at_commit: 6e2995f8
generated_at_date: 2026-11-09
covers_paths: [core/src/shell/exec.rs, core/src/shell/exec/]
---

# Map: core / shell / exec

`core/src/shell/exec/` is the OS plumbing the CBPV [[map/core/evaluator|machine]]
dispatches into — command execution, pipeline orchestration, and the
per-child confinement choice. It re-enters evaluation only through the
`pipeline::StageEval` the machine passes to `PipeNode::run` (a stage's
`(comp, env)`, run from `pipeline/thread.rs`) — stages carry computation
closures, so the mutual recursion is irreducible, though `shell::exec` names no
`shell::evaluator` item; the evaluator reaches it at
`PipeNode::run` and, dispatching an `Exec` node, at
`dispatch::resolve_command_word` → `run_external`, and the redirect scope
(`redirect::scope`), which the machine's `Frame::Redirect` enters
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

- `dispatch.rs` — `resolve_command_word`, the single site that resolves a
  head (env → handlers → external), and `run_external`, which runs an
  admitted external under its redirects. The machine's `Exec` rule runs every other arm itself. There is no builtin arm: a native table entry is
  an `Env` hit on a native value, and a base-frame row is a `Base` hit on the
  handler stack's base layer, run by the `Exec` rule with the argv slice
  ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
  [[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
  `^name` and a path-bearing head are the external directly, consulting
  neither the env nor the handler stack. **Grant admission is an
  external-command property**: only the `External` arm looks the head up and
  admits it (`Command::lookup`, `Command::admit`) before its redirects open,
  refusing it outright, and carries the admitted `Command` to preparation; the
  env, base, and handler arms pass through.
  Handler and alias thunks are lambdas — a unary `{ |args| … }` or a catch-all
  `{ |name args| … }` — with the calling convention fixed by surface position,
  not inferred from a value's runtime shape
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
- `redirect.rs` — the functions that open a redirect target (`open_write`,
  `open_read`, `install_stdin_redirect`, and `Shell::atomic_write`, which
  every host write below the redirect frame shares), over `path`'s
  `Located::stage` / `open_streaming` (`StagedWrite`, `Streamed`): staging a
  write is the path's, judging and committing it the shell's. `redirect/scope.rs` is the
  `RedirectState` lifecycle (open, route fd 1/2 through the shell's sinks,
  restore, settle each write) and `with_redirects`.
- `capture.rs` — `with_capture` and the audit tee, both over one `SinkScope`
  that restores the replaced sinks on `Drop`. Audit retains 1 MiB per stream
  as a prefix and a byte total, independently of ordinary capture.
- `dispatch.rs::Shell::run_external` is `^program args…` for a host holding
  no syntax: the `Exec` rule's external with no handlers, no redirects and no
  span, never evaluating shell source. The caller supplies its mooring;
  current grants, scope, audit and streams remain in force. The REPL
  external-editor hook uses this entry.
- `record.rs` — `CommandRecord`, an external's one `Observed::Command`, begun
  from an admitted `Command` and ended once; silent, holding nothing,
  when nobody listens at launch. `Command::with_record` is the shape that ends
  it before its frame closes: enter the frame, begin the record, tee the
  streams, locate a failure with the visible head and span, end the record.
  `run_external` and `detach` share it. A failure is a `Reason::process` named
  for the head as dispatched, minted where the child's end is known
  (`command::ending`, `explain_exec_error`); its text is `Reason::describe`. A direct
  external stage outlives its frame, so it carries a `StageFrame` (its `Prov`
  and its record) from launch through collection, completing errors and stops
  before its settlement ends the record.
- `command.rs` — one external command, in pipeline order:
  `Command::from_name` records what to look up (`Request`); `lookup` finds
  the program and checks the grants (`Command<Lookup>`), reporting nothing,
  which is all `help` needs; `admit` reports a refusal and keeps any other
  lookup error as an `Error` for `prepare`, so the 127/126 verdict is
  reported inside the command's record and redirects and beats a bad
  argument. `prepare(args, ownership, shell)` vets argv and builds a
  `Prepared`: the `Launch` and the `Diagnosis` that explains a failed start.
  `Prepared::start` installs an owned `io::child::Connection`, starts the
  drains from their owned readers, then spawns, so a thread creation failure
  leaves no child to tear down; a standalone `run` titles the terminal
  first. `Started` owns the resulting `Spawned`, its `denial_pids` and the
  running `Drains` until `collect` or `register` (`pipeline/collect.rs`)
  consumes it; registration cannot fail. Building describes and starting acts
  ([[design/exec|exec]]). `uutils.rs`
  holds [[map/core/builtins|bundled coreutils]]. `job_control.rs` places a
  standalone command once: `Placement::for_command` picks one of `Joined`,
  `Foreground` (holding the prepared loan), `Inherit`, `Headless` or
  `Unloaned`, from which its `PgidPolicy` and whether the terminal may be its
  stdin follow. A pipeline shares its loan preparation,
  `prepare_terminal_loan`, and its stdin rule, `TerminalInput`.
  - **The program and the 126/127 verdict come from the judged candidate
    fold's walk.** `Command::from_name` snapshots a `Request` without I/O: a
    bundled tool, a launch path, or a bare name with its effective `PATH` and
    `Context::cwd`. For a bare name, `Command::lookup` folds the `PATH`
    candidates in order through `path::search`, which walks each once and
    hands the judgment a `Candidate`. Each candidate's object, absent leaf or
    `failure_trace` is judged before its kind or error is disclosed. The first
    admitted program runs; otherwise the first refusal or 126 says why, and
    only absence yields 127. `Command::admit` records a selected refusal through
    `Shell::exec_refused`; `prepare` reads the checked result without probing again. A path
    head's native target — as written, or expanded by `TildePath::expand` —
    is anchored at the launch cwd with only `.` folded; the launcher runs the
    judged program by its real path, the shown spelling as `argv[0]`
    ([[internals/exec-judgment|exec-judgment]]).
  - **The argv-shape step is one refused set read at two moments.**
    `check_argv` maps each argument's head (`Value::heads`) through
    `RefusedArg::of_head` (`core/src/ty/exec_arg.rs`) and carries the shape's own `remedy`; the
    checker maps the argument's *type* through `RefusedArg::of_ty` before the
    run, so this is the backstop for what a type variable hid from it
    ([[invariants/exec-argv-is-words|exec-argv-is-words]],
    [[decisions/260812_exec-boundary-gated-statically|exec-boundary-gated-statically]]).
  - `command/detach.rs` checks detach permission and then uses the same
    command lookup, admission and preparation (`Ownership::Surrendered`) up to
    the one act that differs, `Prepared::detach`, private to that file:
    the child is born by
    `Launch::spawn_detached` ([[map/core/io-process|io-process]]), so its
    pgid is never observed here and nothing can signal, await, or reap it.
    What replaces the handle is a first-order `{pid, desc}` receipt. The
    birthing frame's projection is rendered into the launch exactly as for a
    child we keep, only with `authority::sandbox::Ownership::Surrendered` dropping the
    parent-death tie, so the survivor's authority is frozen as that frame
    left it and no later frame can widen it — it cannot name the process at
    all ([[map/core/capabilities|capabilities]]). All three of its standard
    descriptors are `/dev/null`. `detach` refuses a head a handler intercepts
    (an arm, the catch-all, or a base frame): a handler runs inside this
    session, so nothing could be detached; its scheme is
    `[String] → F [pid: Int, desc: String]`.
  - **A bundled coreutils/diffutils/ripgrep head is a `Program::Tool`,
    always run as a `ral --ral-bundled-tool <tool>` child** — its inherited
    stdio, env, cwd, process group, and sandbox are the execution context, so it
    threads the same spawn/collector/audit machinery as a host external,
    and behaves identically on Windows where no `.exe` exists to spawn
    ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
    [[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]).
  - `prepare_terminal_loan` gates the terminal handoff on a held
    *terminal lease* (`Shell::terminal_lease` is `Some`), a terminal-bound
    stdout or an explicit loan, and no envelope, not on REPL interactivity —
    so a non-interactive script launched at a terminal holds a `Leased` run and
    foregrounds its interactive children, while an exarch `Denied` tool run
    cannot construct the handoff at all
    ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
    On every platform it prepares the terminal snapshot before spawn and lends
    it to the resulting group afterwards; both guards restore on failure or
    unwind. Pipeline preparation likewise precedes anchor spawn, and only an
    owning pipeline (no enclosing stage) prepares: a stage's child joins its
    group unlent, since dropping a prepared loan restores the terminal, so a
    joined pipeline must never hold one. `TerminalInput` is the
    one rule for what a child at the boundary reads (`Placement::stdin` for a
    lone command, `TerminalInput::for_pipeline` for a pipeline's): a joined group passes its
    owner's source through, a startup tty survives only where the child's group
    can read it, and an unlent owned group reads EOF.
    `Started::collect` kills and collects a child whose terminal handover
    fails; the refusal is the verdict. There is no parking: the reaper
    answers any stop with `SIGCONT`
    ([[internals/pipeline-execution|pipeline-execution]],
    [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
  - Redirects install on the handler arm
    ([[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]). One interpreter, `shell::exec::redirect::scope::RedirectState`, installs them for an external and a block alike ([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).
    `io::child::Connection` binds child streams for standalone commands,
    direct pipeline stages, and externals inside stage threads
    ([[map/core/io-process|io-process]]). Pipeline topology stays in `ByteIn`
    and `ByteOut`: a direct stage consumes its downstream writer through
    `Connection::to_pipe`, while a thread stage makes its own `Sink::Pipe`.
    Parent destinations are borrowed after admission; owned pipe writers move.
    Connection takes the selected `Source` as it stands, so terminal stdin
    permission remains `TerminalInput`'s alone.
  - The read redirect (`< file`), the write redirect (`> / >> / >|`, settled
    `committed`/`aborted`/`failed` at frame teardown), and the exec (Host
    and `BundledTool` completion) each build an `Observation`
    (`core/src/shell/runtime/audit/observation.rs`) and pass it to `Shell::observe`
    (`core/src/shell/runtime/audit/fan_out.rs`), the one fan-out: it reports to the
    run's `Mooring` and to the open audit trail alike, judging neither's
    interest — the host filters the rail. It judges only whether anything
    happened: a write onto the discard device is dropped there
    ([[design/audit|audit]]). Core emits plain `Value::Map`s through
    `Value::from_datum`; a host (exarch) decodes their first-order wire
    form back with `Observation::decode`. The observation *shapes* and their card
    rendering live in [[map/exarch/io-surface|io-surface]]
    ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
- `pipeline/` — pipeline planning and execution; the mechanism, its teardown
  order and its forgiveness rules are described in
  [[internals/pipeline-execution|pipeline-execution]]. A multi-stage
  `CompKind::Pipeline` steps through a `PipeNode { collect, group }`:
  `PipeNode::run` resolves the plan, prepares or joins the group, launches
  every stage onto the collector, rolls a part-way failed launch back, and
  `join`s (`drive`, `end_anchor`, then `fold`). No frame is pushed, so an
  unwinding panic tears the group down through the collector's `Drop`
  ([[map/core/evaluator|evaluator]]). `PipeNode`'s field order is its teardown
  order: the collector drops before the group, so its kill still reaches a pgid
  the anchor pins ([[invariants/a-teardown-kills-before-it-joins|a-teardown-kills-before-it-joins]]).
  A standalone external takes the same collector: `Started::collect` builds
  `CollectState::lone`, registers through the same `Started::register` as a
  pipeline stage, and settles it with `settle_lone`; both live beside
  `CollectState` in `collect.rs`.
  - `resolve.rs` freezes each stage's launch decision once as `StageLaunch`
    (`Direct { command, args }` | `Thread`) from the head's resolution, its
    redirects, and whether a `!{…}` audit captures bytes, so launch reads a
    decision rather than re-deriving a dispatch gate. A stage with redirects is
    a `Thread` stage run through `RedirectState`; a `Direct` stage has none,
    and `Prepared::start` wires its stage pipes without interpreting a redirect
    list. **Nothing about a stage's value enters that decision**: a stage's
    dispatch may not depend on what its neighbours return, or the choice would
    stop being observationally transparent. A `Direct` stage is a process in
    the group; a `Thread` stage is an OS thread whose `Shell` starts from the
    parent's session, and only an external it spawns, at any nesting depth, is
    a process. A bundled (uutils) head routes `Direct` like any external, its
    `ral --ral-bundled-tool` child the image chosen by `Command::prepare`.
    Ordinary application and bind do not enter this runtime.
  - `route.rs`'s `open_stage_routes` allocates every interior edge as an
    operating-system byte pipe from **stage position alone** — the zip of
    `Parent : ins` against `outs ++ [Parent]` — and no stage is told which one
    it is: every stage reports what it settled, and the fold, running in launch
    order, keeps the last `Ok`, which is the final stage's whenever that stage
    is `Ok`
    ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
    A non-final stage's returned value is discarded, never serialised onto an
    edge; a `Thread` stage's final value rides home on its own
    `StageObservation`, no wire in between.
  - `pipeline.rs` is the spine: `PipeNode::run` is resolve, group, collector,
    `launch`, one `spawn_stage` per stage pushed onto the collector, then,
    after a `roll_back` if that failed, `join`. Only an owning pipeline (no
    enclosing stage) prepares and lends a `TerminalLoan`; `prepare_terminal_loan` is
    asked only for that loan. `launch.rs` wires one stage's byte endpoints
    (`stage_stdin` for a thread, `Prepared::start` for a process) and dispatches it on its
    frozen `StageLaunch` (`spawn_stage`, which resolves the stage's stdout
    once for either kind, and `launch_external_stage_direct`); `LaunchCx`
    carries what both read, `holds_terminal` among it.
  - `stage.rs` holds `StageHandle`, which dispatches `cancel`/`cut`/`watch`/
    `listen` over its two kinds, `External(LiveExternal)`/`Thread(ThreadStage)`.
    `cancel` records the cause on its own `sent`, joined by `max`, and tells a
    thread's scope (a process hears a cancellation only as the signal the
    collector sends next); `cut` is that cancel with `ReaderGone` plus the
    external's kill, `Effect::KillStage`'s action once the sentinel
    (`sentinel.rs`) has heard a dead write; `listen` sets the sentinel on the
    outbound edge once, and `held` is declared before `kind`, so a forced end
    drops the hold before it joins. An `External` is a `crate::process::Watch`
    alone, the reaper's own subscription: no dedicated waiter thread, the
    reaper's posting closure putting the exit straight onto the collector's
    channel as `Event::Ended(ix, _)`. `file_external_end` reaps the watch,
    after the stage has left `stages`, so no later `KillStage` can name a
    reaped pid, and releases the held-open edge, leaving an `ExternalEnd
    { drain: Drain, settlement: Settlement }` to the fold: `Drain` is the jail
    and the pumps, which wait on the group kill; `Settlement` is what the
    ending reads.
  - `group.rs` — `PipelineGroup` is `Owns { anchor, .. }` or `Joins(Membership)`.
    `prepare` spawns the pgid anchor (`--ral-pipeline-anchor`, ignoring
    `SIGTSTP`/`SIGTTIN`/`SIGTTOU` and immune to termination signals) on every
    platform before any stage exists. On Unix it waits for a zero readiness
    byte after the handlers are installed, and starts its witness in the same
    act: a report reader blocking to EOF on the anchor's stdout, posting
    `Event::Heard(signal)` raw per swallowed signal, and the anchor's own
    `Watch` posting `Event::AnchorDied`. `group()` answers `Group::Owns` for an
    owner and `Group::Joins(membership)` for a joiner, which lends no terminal
    on its own account; `joining` is the no-anchor shape a nested pipeline in a
    stage thread gets, and `membership` is what a thread stage's `Seat`
    carries: an owner's leader paired with the stage's scope, or a joiner's
    own. `AnchorProcess::end` closes the release pipe and joins the report
    reader; dropping the anchor reaps it. The group's verbs are lifecycle only —
    `prepare`/`joining`, `group`, `membership`, `leader_pgid`, `end_anchor`,
    `Drop`: signalling and killing belong to the collector, which holds the
    live stages ([[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]]).
    On Windows an owned group also holds a `Job`.
  - `thread.rs` — `launch_thread_stage` wires a `Thread` stage's `Io` from its
    stdin route and resolved stdout; its closure, given its `Slot { ix, tx }`
    and wrapped in a `SettleOnDrop`, builds its own `StageObservation` and
    sends `Event::Returned(ix, obs)` as its last act, the guard armed until
    then so an unwind reports in its place. A non-final stage's `stdout` is its
    own edge; a **final** stage's is the parent's, so a discarded statement in
    the final stage writes where an enclosing capture's buffer
    (`shell.io.stdout`) is ([[design/capture|capture]]). A captured final stage
    is a `Capture` node, so it runs as a thread stage. `ThreadStage` is the
    collector's handle onto the running thread: `cancel` writes the cause on
    its node (on Windows also one `CancelSynchronousIo`, re-issued while the
    stage's chain carries a cause), `join_after_settled` reclaims a thread
    whose `Returned` has arrived, and `Drop` cancels and joins one never
    observed, a slice at a time.
  - `collect.rs` — `CollectState` owns the stage handles from the first one
    launched and an `mpsc` channel every producer feeds: the reaper, a stage
    thread's own report, the sentinel, the anchor's witness and its watch. `recv`
    first sweeps the edges (`sweep_edges`), then reads the current scope between
    slices and mints `Event::Cancelled` itself. The events are `Ended`,
    `Returned`, `Wrote`, `Cancelled`, `AnchorDied` and `Heard`; there is no
    `Stopped` variant, the reaper having answered a stop already. The pure fold
    `step` returns at most one `Effect` (`KillStage`, `CancelAll { cause }`)
    for the thin interpreter `CollectState::run` to perform; `drive` is `while
    live() { recv; step; run }`, with no interval and no backoff. Completion is
    not an effect: the pipeline is finished exactly when no stage handle is
    left. A `Heard` key strikes the run's node and records `pressed`, but only
    through the collector's `loan: Option<TerminalLoan>`, the one ear for a key.
    `cancel_all(cause)` is the teardown, in the order its home page fixes;
    `CollectState::drop` writes `Terminated` on every live thread stage and
    kills whenever it is dropped with a stage still unobserved. `fold` opens
    with `vacate`, which hears out every queued report and returns the loan
    before any pump is joined, then settles each external through
    `StageEnd::settle` and `Settlement::observe`: the one place `&mut Shell`
    reaches an external's settlement (audit synthesis, exit-hint lookup,
    sandbox-denial augmentation). The verdicts join by `stronger` over
    `Break::rank`, an escape outranking an error and ties going to the earlier
    stage; `drive`, `cancel_all` and `step` take no `&Shell` at all. Which
    stops the collector forgives is fixed in
    [[internals/pipeline-execution|pipeline-execution]]. The `Slot` a stage
    hands its own producers is minted by `launch` from the sending half
    `PipeNode::run` holds and drops once the stages are launched; the
    collector keeps none.
  - `anchor.rs` is the hidden `--ral-pipeline-anchor` child's entrypoint
    (`Role::PipelineAnchor`; a bundled tool's `Role::BundledTool` is launched
    beside it by `authority::sandbox::reexec::launch`). On Windows every
    external a stage thread spawns still resolves `PgidPolicy::Join` against
    the anchor's registered Job Object, assigned at creation under the
    suspended create → assign → resume path
    ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).
- **A `grant` is a dynamic effect scope, not a process boundary, so the grant
  body always evaluates locally** — the machine steps its body in process, with
  no router in between:
  nested grants compose by intersection over authority, an algebra of the
  evaluator's dynamic context. Confinement happens elsewhere — the
  RAL-owned effects are decided in process by `authority::guard::check_*`
  ([[map/core/capabilities|capabilities]]), and child-owned effects are
  kernel-backed at *external dispatch*: when a projection is active,
  `Command::prepare` obtains a confined `process::Launch` via
  `sandboxed_command` — per-command Seatbelt /
  bwrap confinement on Unix; on Windows the LowBox token of the
  projection's own AppContainer profile, carrying the per-path fs
  capability SIDs its projection names
  ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]) —
  and net/fs fail-closed fires when a child is
  actually spawned, not at grant-body entry
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
  Inside a *guest*, preparation takes neither projection branch:
  `jail::guest()` marks every spawn as already confined by the spawn
  jail: a bounded fresh uid, a per-exec cgroup, the envelope's seccomp
  deny-set, and close-on-exec marking of every descriptor above stderr.
  An offline command also gets a fresh network namespace with loopback up;
  IP and outside abstract Unix sockets are out of reach, while pathname
  Unix sockets remain reachable through the filesystem.
  `process/jail.rs` ([[map/core/io-process|io-process]]) carries the plan,
  and `jail/linux.rs` enforces it before the pgid and detach hooks, since bwrap
  needs the user namespaces the guest boot disables; the in-process guards
  apply unchanged, and the collector's external stage tracks the per-exec `JailCgroup`, so
  cancel and settle kill the whole tree through `cgroup.kill` (a
  `setsid`'d grandchild cannot leave its cgroup) while the grace phase
  stays pgid-addressed (`docs/SPEC.md` §12.11).
`core/src/seed.rs` (crate root, beside the wire layer it rides, *not*
under `shell/exec/`) carries only `EngineSeed` and `seal`/`unseal`, the engine seat's
own seed wire for a sealed child engine; a pipeline stage never
crosses a wire at all and this module carries no pipeline-stage type
([[decisions/260902_stages-are-threads|stages-are-threads]],
[[decisions/260610_child-eval-unification|child-eval-unification]], superseded).

The `Shell` state these thread is [[map/core/shell-state|shell-state]]; the serde
mirror and wire envelope they ride is [[map/core/transport|transport]].
