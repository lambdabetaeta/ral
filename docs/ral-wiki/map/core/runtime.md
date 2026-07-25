---
generated_at_commit: f7cf93a
generated_at_date: 2026-07-25
covers_paths: [core/src/runtime.rs, core/src/runtime/, core/src/child_eval.rs]
---

# Map: core / runtime

`core/src/runtime/` is the OS plumbing the CBPV [[map/core/evaluator|machine]]
dispatches into — command execution, pipeline orchestration, and the
per-child confinement choice. It re-enters evaluation only through `call::invoke`,
`eval_block`, and `absorb_tail`; the evaluator reaches it at
`pipeline::run_pipeline`, `command_call::run_call`, and the `command` redirect
guards
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

- `command_call.rs` — `run_call`, the single site that resolves a head
  (env → builtins → handlers → external) and runs the chosen arm; the
  evaluator's down-seam for a bare command. **Grant admission is an
  external-command property**: only the `External` arm consults
  `capability::admits_head` before any argument evaluates, refusing the head
  outright; env, builtin, and handler arms pass through. Handler and alias
  thunks are lambdas — a unary `{ |args| … }` or a catch-all `{ |name args| … }`
  — with the calling convention fixed by surface position, not inferred from a
  value's runtime shape
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
- `command.rs` — the External arm: vet the resolved identity, choose its
  *exec image*, wire stdio per the call's redirects, spawn, and reap.
  Submodules: `identity.rs` (`CommandIdentity`, the classify-once
  name/shown/resolved triple), `vet.rs` (existence → argv shape → grant policy,
  yielding a `SpawnPlan` with its `ExecImage`), `process.rs`, `child.rs`,
  `stdio.rs`, `redirect.rs`, `foreground.rs`, `io_event.rs`, plus `uutils.rs`
  for [[map/core/builtins|bundled coreutils]].
  - **A bundled coreutils/diffutils/ripgrep head is an `ExecImage::BundledTool`,
    run as a `ral --ral-bundled-tool <tool>` child whenever process semantics
    are required** — its inherited stdio, env, cwd, process group, and sandbox
    are the execution context, so it threads the same spawn/`RunningChild`/audit
    machinery as a host external, and behaves identically on Windows where no
    `.exe` exists to spawn
    ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]).
    The inline `uumain` placement (`run_uutils_in_process`) survives only for
    the clean-terminal case: `can_run_uutils_in_process` gates it on *terminal*
    stdin (`Source::Terminal`, never `Pipe`/`File`), direct terminal/stderr
    sinks, no redirects, no capture/audit tee, no env overrides, no
    `within [dir: …]`, no logical/process cwd mismatch, and no active sandbox
    projection. A non-terminal stdin forces child placement rather than the
    parent rewiring its own fd 0; `INLINE_UUTILS_LOCK` serialises the
    reset → invoke → read window so the process-global uucore exit-code cell
    cannot interleave across the REPL thread and a `spawn`/`watch`/`par` worker.
  - `foreground.rs`'s `ForegroundDecision::for_standalone` gates the terminal
    handoff on a held *terminal lease* (`Shell::terminal_lease` is `Some`) plus
    top-level launch role and a terminal-bound sink with no shell-side pump, not
    on REPL interactivity — so a non-interactive script launched at a terminal
    holds a `Leased` run and foregrounds its interactive children, while an
    exarch `Denied` tool run cannot construct the handoff at all
    ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
    Parking a stopped child as a resumable job stays REPL-only.
  - Redirects install on the handler arm
    ([[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]).
    Child stdio routing — the `(Stdio, pump)` plan for a spawned child's
    stdout/stderr — is one shape, `Sink::child_stdout` / `child_stderr` yielding
    the shared `ChildStdioPlan` ([[map/core/io-process|io-process]]), through
    which the standalone command, the direct pipeline stage, and the ral helper
    stage all route; the tty-inherit predicate (`stdio::inherit_tty`) is
    likewise shared.
  - `io_event.rs` fixes the wire shape of the runtime I/O doors: the read door
    (`< file`), the write door (`> / >> / >|`, settled `committed`/`aborted`/
    `failed` at frame teardown), and the exec door (Host and spawned/inline
    `BundledTool` completion). Core emits plain `Value::Map`s onto the run's
    `surface` sink; a host (exarch) decodes them into cards. The event *shapes*
    and their card rendering live in [[map/exarch/io-surface|io-surface]]
    ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
- `pipeline/` — pipeline planning and execution
  (`pipeline.rs`'s `run_pipeline` is the few-line orchestrator: resolve →
  launch → collect). `resolve.rs` reads each stage's channel signature off the
  checker's ground `Wire` and freezes the launch decision once as `StageLaunch`
  (`Direct` | `HelperEval`), so launch reads a decision rather than re-deriving
  a dispatch gate ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]).
  A `PureValue` pipeline (no byte edge anywhere) reduces to a data-last fold in
  the parent evaluator; a `ProcessStaged` pipeline (≥1 byte edge) launches its
  byte-capable stages as subprocesses in one process group.
  - A bundled (uutils) head that carries a value edge routes `HelperEval`, since
    data-last application is evaluator work; a byte-only bundled head routes
    `Direct`, where its `ral --ral-bundled-tool` child is the image chosen by
    `command::build_command` — nothing in the pipeline distinguishes a bundled
    head from a host binary, so both classify as `External` carrying the
    resolved `CommandIdentity`. The value-edge judgment — stage `i` of `n`
    receives a value iff `i > 0 && input ≠ Bytes`, emits one iff
    `i + 1 < n && output ≠ Bytes` — has one definition,
    `route::value_edge_in` / `value_edge_out` beside the `open_stage_routes`
    edge allocator that realises it; `resolve::carries_value_edge` is their
    disjunction. The terminal-ownership decision (`resolve_terminal_plan`)
    likewise gates on a reachable terminal lease and a terminal-bound final
    sink, not on a `startup_foreground` predicate.
  - `launch.rs` (`PipelineBuild` / `PipelineResources` own launch and
    gate-first abort teardown), `group.rs` (the pgid anchor is forked only for a
    multi-stage pipeline, since a single stage leads its own group on spawn),
    `stage.rs` (helper-stage launch + observe), `collect.rs`, `helper.rs` (the
    hidden `--ral-pipeline-stage-helper` / `--ral-pipeline-anchor` /
    `--ral-bundled-tool` child entrypoints and their value-channel codec), and
    `protocol/` (`common.rs`, `unix.rs` / `windows.rs`, `fallback.rs`) for the
    ral⇄ral stage frames — `HelperProtocol::wire` clears an absent
    value-channel env var with `env_remove`, so a spawned helper's protocol vars
    describe exactly that spawn. On Windows the protocol pushes helper handles
    into `process::Launch`, whose raw `CreateProcessW` backend admits them with
    `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; `PipelineGroup` prepares Job Objects
    before spawn and registers only children already assigned before resume
    ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]). Value
    edges are data-last application, the producer
    forced once by `force_pipe_value`
    ([[decisions/260609_pure-pipe-equation|pure-pipe-equation]]).
- **A `grant` is a dynamic effect scope, not a process boundary, so the grant
  body always evaluates locally** — the boundary verbs (`eval_top_level` /
  `eval_block`) run their body in process, with no router in between
  ([[decisions/260610_value-edge-locality|value-edge-locality]]):
  nested grants compose by intersection over authority, an algebra of the
  evaluator's dynamic context. Confinement happens elsewhere — the
  RAL-owned effects are decided in process by `capability::check_*`
  ([[map/core/capabilities|capabilities]]), and child-owned effects are
  kernel-backed at *external dispatch*: when a projection is active,
  `command::build_command` obtains a confined `process::Launch` via
  `projection_enforceable` / `sandboxed_command` — per-command Seatbelt /
  bwrap confinement on Unix; on Windows the LowBox token of the
  projection's own AppContainer profile
  ([[decisions/260713_projection-keyed-appcontainer|projection-keyed]]) —
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
  stays pgid-addressed (`docs/SPEC.md` §15.2).
- `core/src/child_eval.rs` (crate root, beside the wire layer it rides, *not*
  under `runtime/`) — the one re-exec'd-child eval runner the pipeline stage
  helper drives, `run_child_eval(request, upstream, force_output)`
  ([[decisions/260610_child-eval-unification|child-eval-unification]]). One
  request frame in, one response frame out: the child packs the body plus a
  `WireMobile` snapshot, rebuilds its shell with `subprocess::reexec_child_shell`,
  applies `body` to the upstream value data-last via `call::invoke`, drains its
  audit fragment, and ships a single `ChildEvalResponse`. `force_output` forces
  the body's value once before it ships (`x | f = f !{x}`), via the shared
  `pipeline::force_pipe_value`; `wants_value` separately gates whether that
  value is *serialised* into the response, so a byte-mode stage cannot fail the
  run over an incidental non-transferable retained value. `transfer_error` is
  one `pub(crate)` constructor here, re-phrasing a value-serialisation failure
  as a process-boundary error, shared by both the response edge and the pipeline
  value edge.

The `Shell` state these thread is [[map/core/shell-state|shell-state]]; the serde
mirror and wire envelope they ride is [[map/core/transport|transport]].
