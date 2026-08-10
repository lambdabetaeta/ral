---
generated_at_commit: 95449d4
generated_at_date: 2026-08-10
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
  (env → handlers → external) and runs the chosen arm; the evaluator's
  down-seam for a bare command. There is no builtin arm: a fixed-arity manifest
  entry is an `Env` hit on a native value, and a variadic one is a `Base` hit on
  the handler stack's base layer, run by `run_base_frame` with the argv slice
  ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]).
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
    Parking a stopped child as a resumable job stays REPL-only.
  - Redirects install on the handler arm
    ([[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]).
    Child stdio routing — the `(Stdio, pump)` plan for a spawned child's
    stdout/stderr — is one shape, `Sink::child_stdout` / `child_stderr` yielding
    the shared `ChildStdioPlan` ([[map/core/io-process|io-process]]), through
    which the standalone command, the direct pipeline stage, and the ral helper
    stage all route; the tty-inherit predicate (`stdio::inherit_tty`) is
    likewise shared.
  - The read door (`< file`), the write door (`> / >> / >|`, settled
    `committed`/`aborted`/`failed` at frame teardown), and the exec door (Host
    and `BundledTool` completion) each build an `Observation`
    (`core/src/types/observation.rs`) and pass it to `observe`
    (`core/src/evaluator/audit.rs`), the one fan-out door: it reports to the
    run's `Mooring` and to the open audit trail alike, judging neither — the
    host filters the rail. Core emits plain `Value::Map`s through
    `Observation::to_value`; a host (exarch) decodes them back with
    `Observation::from_value`. The observation *shapes* and their card
    rendering live in [[map/exarch/io-surface|io-surface]]
    ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
- `pipeline/` — pipeline planning and execution
  (`pipeline.rs`'s `run_pipeline` is the few-line orchestrator: resolve →
  launch → collect). `resolve.rs` freezes each stage's launch decision once as
  `StageLaunch` (`Direct` | `HelperEval`) from the head's resolution, redirects,
  terminal ownership, and audit state, so launch reads a decision rather than
  re-deriving a dispatch gate. **No route enters that classification**: a
  stage's dispatch may not depend on where its payload lives, or the choice
  would stop being observationally transparent. The single type-level fact
  resolve carries is `PipelinePlan::final_route`, the checker's `GroundRoute`
  for the last stage. `route.rs`'s `open_stage_routes` then allocates every
  interior edge as an operating-system byte pipe from **stage position alone**
  (`i + 1 < n`), and derives `FinalValue::Report` from `i + 1 == n` together
  with that one `final_route` — the pipeline's only value-transport question
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  A non-final stage's returned value is discarded, never serialised onto an
  edge. A multi-stage pipeline always launches its stages as subprocesses in one
  process group; ordinary application and bind do not enter this runtime.
  - A bundled (uutils) head routes `Direct` like any external, its
    `ral --ral-bundled-tool` child the image chosen by `command::build_command`
    — nothing in the pipeline distinguishes a bundled head from a host binary,
    so both classify as `External` carrying the resolved `CommandIdentity`.
    Value-style composition is evaluator application, not a helper route. The
    terminal-ownership decision (`resolve_terminal_plan`) likewise gates on a
    reachable terminal lease and a terminal-bound final sink, not on a
    `startup_foreground` predicate.
  - `launch.rs` (`PipelineBuild` / `PipelineResources` own launch and
    gate-first abort teardown), `group.rs` (the pgid anchor is forked only for a
    multi-stage pipeline, since a single stage leads its own group on spawn),
    `stage.rs` (helper-stage launch + observe), `collect.rs`, `helper.rs` (the
    hidden `--ral-pipeline-stage-helper` / `--ral-pipeline-anchor` /
    `--ral-bundled-tool` child entrypoints and their final-report helper), and
    `protocol/` (`common.rs`, `unix.rs` / `windows.rs`, `fallback.rs`) for the
    ral⇄ral stage frames. The helper protocol carries gate and final-report
    frames; it carries no typed value between interior stages. On Windows the
    protocol pushes helper handles into `process::Launch`, whose raw
    `CreateProcessW` backend admits them with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`;
    `PipelineGroup` prepares Job Objects before spawn and registers only children
    already assigned before resume
    ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).
- **A `grant` is a dynamic effect scope, not a process boundary, so the grant
  body always evaluates locally** — the boundary verbs (`eval_top_level` /
  `eval_block`) run their body in process, with no router in between:
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
  stays pgid-addressed (`docs/SPEC.md` §15.2).
- `core/src/child_eval.rs` (crate root, beside the wire layer it rides, *not*
  under `runtime/`) — the one re-exec'd-child eval runner the pipeline stage
  helper drives, `run_child_eval` ([[decisions/260610_child-eval-unification|child-eval-unification]]).
  One request frame in, one response frame out: the child packs the body plus a
  `WireMobile` snapshot, rebuilds its shell with `subprocess::reexec_child_shell`,
  evaluates the stage against its byte input, drains its audit fragment, and
  ships a single `ChildEvalResponse`. When the pipeline's `final_route` is
  `Value`, `FinalValue::Report` asks this helper response to carry the value;
  the final report remains helper-staged until a separate in-parent-tail
  decision. The response frame travels its own socketpair, never aliased with an
  interior pipe, so there is no upstream typed-value edge.

The `Shell` state these thread is [[map/core/shell-state|shell-state]]; the serde
mirror and wire envelope they ride is [[map/core/transport|transport]].
