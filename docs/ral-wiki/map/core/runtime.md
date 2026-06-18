---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [core/src/runtime.rs, core/src/runtime/, core/src/child_eval.rs]
---

# Map: core / runtime

`core/src/runtime/` is the OS plumbing the CBPV [[map/core/evaluator|machine]]
dispatches into — command execution, pipeline orchestration, and the
process-dispatch choice. It re-enters evaluation only through `call::invoke`,
`eval_block`, and `absorb_tail`; the evaluator reaches it at
`pipeline::run_pipeline`, `command_call::run_call`, the `command` redirect
guards, and `transport::dispatch`
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

- `command/` — external-command dispatch: `identity.rs` (`CommandIdentity`, the
  classify-once name/shown/resolved triple), `vet.rs`, `process.rs`, `child.rs`,
  `stdio.rs`, `redirect.rs`, `foreground.rs`, plus `uutils.rs` for
  [[map/core/builtins|bundled coreutils]]. `foreground.rs`'s `ForegroundDecision`
  gates the terminal handoff on `startup_foreground` (owning the terminal's
  foreground) rather than REPL interactivity, so a non-interactive script
  launched at a terminal foregrounds its interactive children; parking on stop
  stays REPL-only
  ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
  Redirects install on the handler arm
  ([[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]).
  Child stdio routing — the `(Stdio, pump)` plan for a spawned child's
  stdout/stderr — is one shape, `Sink::child_stdout` / `child_stderr` yielding
  the shared `ChildStdioPlan` ([[map/core/io-process|io-process]]), through which
  the standalone command, the direct pipeline stage, and the ral helper stage all
  route; the tty-inherit predicate (`stdio::inherit_tty`) is likewise shared.
- `command_call.rs` — `run_call`, the single site that classifies a head and
  runs it; the evaluator's down-seam for a bare command.
- `pipeline/` — pipeline planning and execution. `resolve.rs` reads each stage's
  channel signature off the checker's ground `Wire` and freezes the launch
  decision once as `StageLaunch` (`Direct` | `HelperEval`), so
  launch reads a decision rather than re-deriving a dispatch gate
  ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]). A
  bundled (uutils) head that carries a value edge routes `HelperEval`, since
  data-last application is evaluator work; a byte-only bundled head routes
  `Direct` as a `ral --ral-bundled-tool` child placement
  ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]),
  so it cannot perform the data-last application a value edge demands. The
  value-edge judgment — stage `i` of `n` receives a value iff `i > 0 && input ≠
  Bytes`, emits one iff `i + 1 < n && output ≠ Bytes` — has one definition,
  `route::value_edge_in` / `value_edge_out` beside the `open_stage_routes` edge
  allocator that realises it; `resolve::carries_value_edge` is their disjunction.
  `launch.rs` (`PipelineBuild` / `PipelineResources` own launch and gate-first
  abort teardown), `group.rs` (the pgid anchor is forked only for `n ≥ 2`, since
  a single stage leads its own group on spawn), `stage.rs` (helper-stage launch
  + observe), `collect.rs`, `helper.rs` (the stage-helper child shell built
  through `subprocess::reexec_child_shell`), and `protocol/` (`common.rs`,
  `unix.rs` / `windows.rs`) for the ral⇄ral stage frames — `HelperProtocol::wire`
  clears an absent value-channel env var with `env_remove`, so a spawned helper's
  protocol vars describe exactly that spawn. Value edges are data-last
  application, the producer forced once by `force_pipe_value`
  ([[decisions/260609_pure-pipe-equation|pure-pipe-equation]]).
- `transport.rs` — chooses in-process vs OS-sandboxed-child dispatch, orthogonal
  to the block contract (`eval_block`); the sandboxed path is
  [[map/core/capabilities|grant confinement]].
- `core/src/child_eval.rs` (crate root, beside the wire layer it rides, *not*
  under `runtime/`) — the one re-exec'd-child eval runner shared by the pipeline
  stage and the sandbox, `run_child_eval(request, upstream, ChildKind)`
  ([[decisions/260610_child-eval-unification|child-eval-unification]]). The
  forcing decision rides the eval-shape payload, not the wire: `ChildKind::PipelineStage`
  carries `force_output: bool`, derived at the serve site from the value-out
  channel's presence (`value_out.is_some()`), so the channel is the single source
  of truth for whether the body's value is forced once before it crosses the
  edge. `transfer_error` is one `pub(crate)` constructor here, re-phrasing a
  value-serialisation failure as a process-boundary error, shared by both the
  remote-eval response edge and the pipeline value edge.

The `Shell` state these thread is [[map/core/shell-state|shell-state]]; the serde
mirror and wire envelope they ride is [[map/core/transport|transport]].
