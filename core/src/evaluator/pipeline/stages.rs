//! Pipeline stages: analysis, launching, and result collection.
//!
//! All per-stage machinery the coordinator in `pipeline.rs` delegates to.
//! Three sections in source order, executed once each per pipeline run:
//!
//! 1. **Analysis** — type-infer every stage, validate adjacency, eagerly
//!    evaluate external argv.
//! 2. **Launch** — spawn either an OS process or an evaluator thread,
//!    wiring inter-stage byte/value channels.
//! 3. **Collect** — join handles in order and assemble the final value.

use std::mem;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use super::super::{audit, dispatch, eval_comp, exec};
use super::group::{PipelineGroup, PipelineMode};
use crate::io::{Sink, Source};
use crate::ir::{Comp, CompKind, ExecName, Val};
use crate::ty::InferCtx;
use crate::types::*;

// ╔═══ Analysis ════════════════════════════════════════════════════════════╗

/// Resolved name and pre-evaluated argument values for an external stage.
///
/// Args are kept as `Value`s rather than strings so launch-time
/// `resolve_command` can run the same `reject_exec_arg` checks that
/// `exec_external` runs (lists/maps/thunks/handles/Bytes are rejected
/// with a hint to use `...$xs` or `to-bytes`).
#[derive(Clone, Debug)]
pub(super) struct ExternalStage {
    pub(super) name: ExecName,
    pub(super) args: Vec<Value>,
}

/// How a pipeline stage will be executed.
///
/// The classification matches `dispatch_by_name` so that effect handlers,
/// `^name`, aliases, builtins, grant denials, and stage-level redirects all
/// behave identically inside and outside a pipeline.  `External` is the fast
/// path — direct fork/exec, no shell logic in between.  `Internal` runs the
/// stage in an evaluator thread that re-enters the normal dispatch chain via
/// `eval_comp`; that's how handler interception, builtin invocation, alias
/// expansion, and per-stage redirects are all picked up.
#[derive(Clone, Debug)]
pub(super) enum StageDispatch {
    /// Run the stage in an evaluator thread via `eval_comp`.
    Internal,
    /// Spawn the resolved external command directly.
    External(ExternalStage),
}

impl StageDispatch {
    pub(super) fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }
}

/// Per-stage analysis result: resolved comp type, dispatch verdict, and
/// source position.
#[derive(Clone, Debug)]
pub(super) struct StageSpec {
    pub(super) comp_type: crate::ty::CompType,
    pub(super) dispatch: StageDispatch,
    pub(super) line: usize,
    pub(super) col: usize,
}

/// Wrap an I/O error from pipe creation or cloning into an `EvalSignal`.
fn pipe_error(e: impl std::fmt::Display) -> EvalSignal {
    EvalSignal::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Create an OS pipe pair, mapping failure to `EvalSignal`.
fn create_pipe() -> Result<(os_pipe::PipeReader, os_pipe::PipeWriter), EvalSignal> {
    os_pipe::pipe().map_err(pipe_error)
}

/// Extract source position from a pipeline stage, converting the span's byte
/// offset into (line, col) via the current source text held on `shell`.
fn stage_position(stage: &Comp, shell: &Shell) -> (usize, usize) {
    match (stage.span, shell.location.source.as_deref()) {
        (Some(sp), Some(src)) => crate::diagnostic::byte_to_line_col(src, sp.start as usize),
        (Some(sp), None) => (sp.start as usize, 0),
        (None, _) => (0, 0),
    }
}

/// Extract the command name from the head of a pipeline stage, if it has one.
///
/// Returns `Some` for `Exec` nodes (external commands) and `App` nodes whose
/// head is a forced variable (function calls).  Returns `None` for anything
/// else (anonymous computations, literals, etc.).
enum StageHead<'a> {
    Exec(&'a ExecName),
    App(&'a str),
}

fn stage_head_name(stage: &Comp) -> Option<StageHead<'_>> {
    match &stage.kind {
        CompKind::Exec { name, .. } => Some(StageHead::Exec(name)),
        CompKind::App { head, .. } => match &head.as_ref().kind {
            CompKind::Force(Val::Variable(name)) => Some(StageHead::App(name)),
            _ => None,
        },
        _ => None,
    }
}

/// Decide whether a pipeline stage takes the External fast path or routes
/// through the Internal (evaluator-thread) path.
///
/// Defers the entire handler / `^name` / alias / builtin / grant decision
/// to the shared `dispatch::classify_dispatch` so pipeline stages and
/// single commands cannot drift.  The pipeline-only concern — stage-level
/// redirects — forces Internal up front so `eval_comp -> dispatch_by_name
/// -> exec_external` applies the redirects.
fn classify_stage_dispatch(stage: &Comp, shell: &Shell) -> DispatchKind {
    let CompKind::Exec {
        name,
        redirects,
        external_only,
        ..
    } = &stage.kind
    else {
        return DispatchKind::Internal;
    };

    // Stage-level redirects: defer to exec_external via the Internal path.
    if !redirects.is_empty() {
        return DispatchKind::Internal;
    }

    match dispatch::classify_dispatch(name, *external_only, shell) {
        dispatch::Dispatch::External => DispatchKind::External,
        // Handler / Alias / Builtin / GrantDenied all need ral code to run
        // — let the evaluator thread re-enter dispatch_by_name and execute
        // them through the single source of truth.
        _ => DispatchKind::Internal,
    }
}

/// Verdict from `classify_stage_dispatch`; resolved into a `StageDispatch`
/// (with eager argv evaluation) by `analyze_stage`.
enum DispatchKind {
    Internal,
    External,
}

/// Analyze a single pipeline stage.
///
/// Resolves the head name against the environment to obtain a channel-type
/// signature, classifies the stage's dispatch path (mirroring
/// `dispatch_by_name`), and eagerly evaluates argv for the External fast path.
fn analyze_stage(
    stage: &Comp,
    shell: &mut Shell,
    ctx: &mut InferCtx,
) -> Result<StageSpec, EvalSignal> {
    let (line, col) = stage_position(stage, shell);
    let Some(name) = stage_head_name(stage) else {
        return Ok(StageSpec {
            comp_type: ctx.comp_type(stage, None),
            dispatch: StageDispatch::Internal,
            line,
            col,
        });
    };

    let hit = match name {
        StageHead::Exec(name) => name
            .bare()
            .map(|name| ctx.resolve_head(name, Some(shell)))
            .unwrap_or(crate::ty::HeadResolution {
                comp_type: crate::ty::CompType::ext(),
                internal: false,
            }),
        StageHead::App(name) => ctx.resolve_head(name, Some(shell)),
    };

    let dispatch = match (classify_stage_dispatch(stage, shell), &stage.kind) {
        (DispatchKind::External, CompKind::Exec { name, args, .. }) => {
            let vals = dispatch::eval_call_args(args, None, shell)?;
            StageDispatch::External(ExternalStage {
                name: name.clone(),
                args: vals,
            })
        }
        // App stages and any non-Exec — always Internal.
        _ => StageDispatch::Internal,
    };

    Ok(StageSpec {
        comp_type: hit.comp_type,
        dispatch,
        line,
        col,
    })
}

/// Construct a diagnostic for a channel mode mismatch between adjacent stages.
///
/// Produces a hint suggesting an explicit conversion command (e.g. `from-json`,
/// `to-lines`) to bridge the gap.
fn pipeline_mismatch(
    line: usize,
    col: usize,
    got: crate::ty::Mode,
    expected: crate::ty::Mode,
) -> EvalSignal {
    let (message, hint) = match (got, expected) {
        (crate::ty::Mode::None, crate::ty::Mode::Bytes) => (
            "pipeline channel mismatch: value stage cannot feed byte stage",
            "to decode a bytes value, pass it as an argument: `read-string $var`; or insert `to-lines` to encode a list as text",
        ),
        (crate::ty::Mode::Bytes, crate::ty::Mode::None) => (
            "pipeline channel mismatch: byte stage cannot feed value stage",
            "insert `from-json`, `from-lines`, or another from-X command to decode the byte stream",
        ),
        _ => unreachable!(),
    };
    EvalSignal::Error(Error::new(message, 1).at(line, col).with_hint(hint))
}

/// Unify the output mode of each stage with the input mode of its successor.
///
/// Fails with a descriptive diagnostic if any adjacent pair has incompatible
/// channel modes (e.g. bytes feeding a value-only consumer).
fn validate_pipeline(specs: &[StageSpec], ctx: &mut InferCtx) -> Result<(), EvalSignal> {
    for i in 1..specs.len() {
        if let Err(e) = ctx
            .unifier
            .unify(specs[i - 1].comp_type.output, specs[i].comp_type.input)
        {
            return Err(pipeline_mismatch(
                specs[i].line,
                specs[i].col,
                e.left,
                e.right,
            ));
        }
    }
    Ok(())
}

/// Frozen output of the resolve phase: per-stage analysis plus the
/// pipeline-level invariants derived from it.  Built once at the start of
/// `run_pipeline` and threaded through launch + collect.
pub(super) struct PipelinePlan {
    pub(super) specs: Vec<StageSpec>,
    pub(super) mode: super::group::PipelineMode,
    pub(super) last_output: crate::ty::Mode,
    pub(super) auditing: bool,
}

/// Resolve phase: type-check every stage, validate channel adjacency,
/// classify dispatch (External fast path vs Internal evaluator-thread),
/// and freeze the pipeline-level mode + last-output mode + audit flag.
///
/// Pure of side effects on `shell` aside from argv evaluation for stages
/// that take the External fast path; no process / pipe is created here.
pub(super) fn resolve_pipeline(
    stages: &[Comp],
    shell: &mut Shell,
) -> Result<PipelinePlan, EvalSignal> {
    let mut ctx = InferCtx::new();
    let mut specs: Vec<StageSpec> = stages
        .iter()
        .map(|s| analyze_stage(s, shell, &mut ctx))
        .collect::<Result<_, _>>()?;
    validate_pipeline(&specs, &mut ctx)?;

    for spec in &mut specs {
        // Default unconstrained mode variables to value-channel (`none`).
        // This keeps higher-order wrappers (like `!{}`) usable while still
        // letting explicit neighboring stages constrain byte-mode as needed.
        for m in [&mut spec.comp_type.input, &mut spec.comp_type.output] {
            *m = ctx.unifier.resolve(*m);
            if m.is_var() {
                *m = crate::ty::Mode::None;
            }
        }
    }

    let pure_external = !specs.is_empty() && specs.iter().all(|s| s.dispatch.is_external());
    let mode = if pure_external {
        super::group::PipelineMode::PureExternal
    } else {
        super::group::PipelineMode::Mixed
    };
    let last_output = specs
        .last()
        .map(|s| s.comp_type.output)
        .unwrap_or(crate::ty::Mode::None);
    let auditing = shell.audit.tree.is_some();

    Ok(PipelinePlan {
        specs,
        mode,
        last_output,
        auditing,
    })
}

// ╔═══ Launch ══════════════════════════════════════════════════════════════╗

/// A live pipeline stage: either an OS process or an evaluator thread.
///
/// The thread variant carries `(result, last_status)` so the last stage's
/// exit code is rejoined into the parent shell — same shape the OS-process
/// variant already exposes via `wait().status`.
pub(super) enum StageHandle {
    Process(ProcessHandle),
    Thread(std::thread::JoinHandle<(Result<Value, EvalSignal>, i32)>),
}

/// A running external-process stage with its pump thread and audit capture.
///
/// `child` is held in an `Option` so that the success path (`join`) and the
/// abort path (`Drop`) can be distinguished structurally: `join` takes the
/// child out, so by the time `Drop` runs the field is `None` and the
/// destructor is a no-op.  If `Drop` runs *without* a successful join — a
/// panic between launches, an early-error return — the child is still
/// `Some`, and the destructor kills the pgid (taking sibling stages with
/// it) and reaps.
pub(super) struct ProcessHandle {
    name: String,
    args: Vec<String>,
    line: usize,
    col: usize,
    child: Option<std::process::Child>,
    /// Pipeline pgid the child belongs to.  Used by `wait_handling_stop`
    /// and by `Drop` to SIGKILL the whole group rather than only the
    /// direct child PID — `/bin/sh -c 'sleep 999'` should not leave the
    /// sleep alive when its parent dies.
    pgid: Option<crate::signal::Pgid>,
    /// Pump thread draining piped stdout into the next stage or final sink.
    /// `None` when stdout is inherited or connected via a direct OS pipe.
    pump: Option<std::thread::JoinHandle<()>>,
    /// stderr drainer thread (only present when auditing pipes stderr).
    /// Captures a bounded prefix and discards the rest, so a child that
    /// fills its stderr pipe cannot deadlock against the stdout pump.
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    /// Capture buffer for auditing; written by the pump's Tee sink.
    audit_capture: Option<Arc<Mutex<Vec<u8>>>>,
    /// Wall-clock start time (µs since epoch); 0 when not auditing.
    start_us: i64,
}

impl Drop for ProcessHandle {
    /// Abort-path cleanup.  Runs only when `join` was *not* called — the
    /// success path takes `child` out, leaving `None`.
    ///
    /// Order: kill the pgid first so the child terminates quickly and the
    /// pump / stderr drainer (which were reading from the child's stdout /
    /// stderr) see EOF; join the drainer threads; then wait to reap.
    /// Killing the pgid (rather than just `child.kill()`) ensures any
    /// descendant of the direct child — `/bin/sh` spawning `sleep 999` —
    /// gets cleaned up too.
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        match self.pgid {
            Some(crate::signal::Pgid(p)) => unsafe {
                libc::kill(-p, libc::SIGKILL);
            },
            None => {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }
        if let Some(jh) = self.stderr_reader.take() {
            let _ = jh.join();
        }
        let _ = child.wait();
    }
}

impl ProcessHandle {
    /// Wait for the child to terminate, then join pump and stderr drainer.
    ///
    /// Order matters: a child stopped by SIGTSTP / SIGSTOP keeps its
    /// stdout / stderr pipes open, so a drainer that joined first would
    /// block forever waiting for EOF — and `wait_handling_stop` would
    /// never run to detect the stop and tear the pgid down.  By calling
    /// the wait helper first we guarantee that either the child exits
    /// cleanly or we SIGKILL its pgid; in both cases the pipes close and
    /// the drainers reach EOF promptly.
    ///
    /// The drainers are running on background threads from spawn time, so
    /// they keep the child's pipes drained while we wait — the child can
    /// still produce output without blocking on a full pipe.  Joining them
    /// after the wait simply collects their results.
    fn join(mut self, shell: &mut Shell, is_last: bool) -> Result<i32, EvalSignal> {
        // Take the child out so Drop becomes a no-op for this handle on
        // the success path; sibling pipeline stages keep running.
        let mut child = self
            .child
            .take()
            .expect("ProcessHandle::join called twice");
        let status = crate::signal::wait_handling_stop(&mut child, self.pgid)
            .map_err(|e| EvalSignal::Error(Error::new(format!("{}: {e}", self.name), 1)))?;

        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }

        let stderr = self
            .stderr_reader
            .take()
            .and_then(|jh| jh.join().ok())
            .unwrap_or_default();
        if !stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&stderr));
        }

        let code = exec::exit_code(status);
        let effective = if !is_last && code == super::SIGPIPE_STATUS {
            0
        } else {
            code
        };

        // Strip one trailing newline from captured stdout — same as $().
        let mut stdout = self
            .audit_capture
            .take()
            .and_then(|b| b.lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        if stdout.last() == Some(&b'\n') {
            stdout.pop();
        }

        let principal = audit::principal(shell);
        if let Some(tree) = &mut shell.audit.tree {
            let mut node = ExecNode::leaf(
                &self.name,
                std::mem::take(&mut self.args),
                effective,
                &shell.location.call_site.script,
                self.line,
                self.col,
            );
            node.stdout = stdout;
            node.stderr = stderr;
            node.start = self.start_us;
            node.end = epoch_us();
            node.principal = principal;
            tree.push(node);
        }

        Ok(effective)
    }
}

pub(super) type ValueResult = Result<Value, EvalSignal>;
pub(super) type ValueRx = std::sync::mpsc::Receiver<ValueResult>;

/// The inter-stage channel carried forward through the pipeline launch loop.
///
/// Each stage consumes the channel it received from its predecessor and
/// produces a new channel for its successor.  Having a single enum (rather
/// than two parallel Options) ensures the two kinds of channel are mutually
/// exclusive and makes the handoff explicit.
pub(super) enum Channel {
    /// No predecessor output is available (first stage, or value-less edge).
    None,
    /// A byte stream from the predecessor's stdout.
    Bytes(os_pipe::PipeReader),
    /// A structured value from the predecessor's evaluation.
    Value(ValueRx),
}

/// Arguments gathered by `run_pipeline` and forwarded into `launch_stage`.
pub(super) struct LaunchContext<'a> {
    pub(super) spec: &'a StageSpec,
    pub(super) i: usize,
    pub(super) specs: &'a [StageSpec],
    pub(super) group: &'a mut PipelineGroup,
    /// Pipeline cancel scope; stamped on internal stage threads.
    pub(super) cancel: crate::signal::CancelScope,
}

/// How the external stage's stdout is routed.
enum StdoutPlan {
    /// Inherit fd 1 directly — final stage to a real terminal with no auditing.
    Inherit,
    /// OS pipe to the next external stage — no pump thread needed.
    DirectPipe,
    /// Pump thread; destination is the next stage's stdin or `shell.io.stdout`.
    Pump,
}

fn plan_stdout(
    is_last: bool,
    next_is_ext: bool,
    stdout_is_terminal: bool,
    auditing: bool,
) -> StdoutPlan {
    if is_last && stdout_is_terminal && !auditing {
        StdoutPlan::Inherit
    } else if !is_last && next_is_ext && !auditing {
        StdoutPlan::DirectPipe
    } else {
        StdoutPlan::Pump
    }
}

fn launch_external_stage(
    spec: &StageSpec,
    is_last: bool,
    next_is_ext: bool,
    shell: &mut Shell,
    incoming: Channel,
    group: &mut PipelineGroup,
    auditing: bool,
) -> Result<(ProcessHandle, Channel), EvalSignal> {
    let external = match &spec.dispatch {
        StageDispatch::External(e) => e.clone(),
        StageDispatch::Internal => {
            unreachable!("launch_external_stage called for internal stage")
        }
    };
    let stdout_plan = plan_stdout(
        is_last,
        next_is_ext,
        shell.io.terminal.startup_stdout_tty
            && matches!(shell.io.stdout, Sink::Terminal | Sink::External(_)),
        auditing,
    );

    // Single resolution path: PATH lookup, grant policy check, and argv
    // rejection (lists/maps/thunks/handles/Bytes) — all the same checks
    // exec_external runs.  Failures here happen before any pipe or process
    // is created.
    let rc = exec::resolve_command(&external.name, &external.args, shell)?;
    let mut cmd = exec::build_command(&rc, shell);

    // Allocate every pipe we will need *before* spawning the child.  Once
    // `cmd.spawn()` returns Ok, the child is owned and any later fallible
    // setup must not leave it unowned.  Pre-spawn allocation is the
    // simplest way to guarantee that invariant for stdout/outgoing pipes;
    // the audit-capture buffer and shell.io.stdout clone are allocated up
    // here for the same reason.
    let mut outgoing_reader: Option<os_pipe::PipeReader> = None;
    let mut pump_sink: Option<Sink> = None;
    let mut audit_capture: Option<Arc<Mutex<Vec<u8>>>> = None;

    cmd.stderr(if auditing {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    // External stages consume only a byte stream from their predecessor.
    // Value channels are dropped, unblocking the sender.  When there is no
    // upstream pipe, choose between inherit and null based on pipeline
    // mode: a pure-external pipeline will own the tty (the leader pgid is
    // foregrounded) so its stages can safely read fd 0; a mixed pipeline
    // keeps the tty with ral, so handing fd 0 to a backgrounded external
    // would SIGTTIN it.  Pre-tty inputs (non-tty stdin) are always safe.
    let stdin_route = match incoming {
        Channel::Bytes(r) => exec::StdinRoute::Pipe(r),
        _ if !shell.io.terminal.startup_stdin_tty => {
            exec::StdinRoute::Inherit(exec::TtyInputPermit::for_non_tty_stdin())
        }
        _ => match group.mode() {
            PipelineMode::PureExternal => {
                exec::StdinRoute::Inherit(exec::TtyInputPermit::for_pure_external_pipeline())
            }
            PipelineMode::Mixed => exec::StdinRoute::Null,
        },
    };
    cmd.stdin(stdin_route.into_stdio());

    match stdout_plan {
        StdoutPlan::Inherit => {
            cmd.stdout(Stdio::inherit());
        }
        StdoutPlan::DirectPipe => {
            let (r, w) = create_pipe()?;
            cmd.stdout(Stdio::from(w));
            outgoing_reader = Some(r);
        }
        StdoutPlan::Pump => {
            cmd.stdout(Stdio::piped());
            let base_sink = if is_last {
                shell.io.stdout.try_clone().map_err(pipe_error)?
            } else {
                let (reader, writer) = create_pipe()?;
                outgoing_reader = Some(reader);
                Sink::Pipe(writer)
            };
            pump_sink = Some(if auditing {
                let cap = Arc::new(Mutex::new(Vec::<u8>::new()));
                audit_capture = Some(cap.clone());
                Sink::Tee(Box::new(Sink::Buffer(cap)), Box::new(base_sink))
            } else {
                base_sink
            });
        }
    };

    group.pre_exec_hook(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| exec::spawn_error(&rc.shown, e))?;
    group.register_child(&child);
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }

    // stderr is `Stdio::piped()` iff `auditing` (see above).  Spawn the
    // drainer immediately so the child cannot deadlock on stderr while the
    // stdout pump waits for EOF.
    let stderr_reader = if auditing {
        exec::spawn_stderr_reader(&mut child)
    } else {
        None
    };

    // Wire up the pump now that the child exists; everything fallible was
    // resolved pre-spawn, so this branch can no longer leak the child.
    let pump = match (pump_sink, child.stdout.take()) {
        (Some(sink), Some(child_stdout)) => Some(sink.pump(child_stdout)),
        _ => None,
    };

    let handle = ProcessHandle {
        name: rc.shown,
        args: rc.args,
        line: spec.line,
        col: spec.col,
        child: Some(child),
        pgid: group.leader_pgid(),
        pump,
        stderr_reader,
        audit_capture,
        start_us: if auditing { epoch_us() } else { 0 },
    };

    Ok((
        handle,
        outgoing_reader.map_or(Channel::None, Channel::Bytes),
    ))
}

type ThreadOutcome = (Result<Value, EvalSignal>, i32);
type InternalStageResult = Result<(std::thread::JoinHandle<ThreadOutcome>, Channel), EvalSignal>;

fn launch_internal_stage(
    stage: &Comp,
    comp_type: crate::ty::CompType,
    is_last: bool,
    next_input: Option<crate::ty::Mode>,
    shell: &Shell,
    incoming: Channel,
    cancel: crate::signal::CancelScope,
) -> InternalStageResult {
    let needs_byte_output = matches!(next_input, Some(crate::ty::Mode::Bytes));
    let needs_value_output = matches!(next_input, Some(crate::ty::Mode::None));

    // Extract the predecessor's channel, dropping it if this stage doesn't
    // consume it (so the sending side is not left blocked).
    let (incoming_stdin, incoming_value_rx) = match incoming {
        Channel::Bytes(r) if comp_type.input == crate::ty::Mode::Bytes => (Some(r), None),
        Channel::Value(rx) if comp_type.input == crate::ty::Mode::None => (None, Some(rx)),
        _ => (None, None), // drop: unblocks sender
    };

    let byte_pipe = if needs_byte_output {
        Some(create_pipe()?)
    } else {
        None
    };
    let value_pipe = if needs_value_output {
        Some(std::sync::mpsc::channel::<ValueResult>())
    } else {
        None
    };

    let stage_comp = stage.clone();
    let snap = shell.snapshot();
    let outer_io = shell.io.try_clone().map_err(pipe_error)?;
    let output_channel = comp_type.output;
    let pipe_writer = byte_pipe
        .as_ref()
        .map(|(_, w)| w.try_clone())
        .transpose()
        .map_err(pipe_error)?;
    let val_tx = value_pipe.as_ref().map(|(tx, _)| tx.clone());

    let handle = shell.spawn_thread(snap, move |child_env| {
        child_env.io.stdout = if output_channel == crate::ty::Mode::Bytes && !is_last {
            pipe_writer.map(Sink::Pipe).unwrap_or(outer_io.stdout)
        } else {
            outer_io.stdout
        };
        child_env.io.terminal = outer_io.terminal;
        child_env.io.interactive = outer_io.interactive;
        // The thread runs inside ral itself — any nested `exec_external`
        // call must NOT take terminal foreground.  Stamp the JobControl
        // permit so `want_fg` is structurally false regardless of the
        // thread's startup_stdin_tty / stdout-Terminal heuristics.
        child_env.io.job_control = crate::io::JobControl::pipeline_thread();
        // Inherit the pipeline's cancel scope so `signal::check` (called
        // between effectful steps) can unwind this thread when the
        // pipeline aborts.  Without this the thread would have a fresh
        // root scope and never observe the parent's cancel.
        child_env.cancel = cancel;
        // incoming_stdin is Some only when input_channel == Bytes (by construction above).
        child_env.io.stdin = incoming_stdin.map(Source::Pipe).unwrap_or(Source::Terminal);
        child_env.io.value_in = None;

        if let Some(rx) = incoming_value_rx {
            match rx.recv() {
                Ok(Ok(v)) => child_env.io.value_in = Some(v),
                Ok(Err(e)) => {
                    if let Some(tx) = val_tx {
                        let _ = tx.send(Err(e.clone()));
                    }
                    return (Err(e), child_env.control.last_status);
                }
                Err(_) => {}
            }
        }

        let result = eval_comp(&stage_comp, child_env);
        if let Some(tx) = val_tx {
            let _ = tx.send(result.clone());
        }
        (result, child_env.control.last_status)
    });

    let outgoing = match (byte_pipe, value_pipe) {
        (Some((reader, _)), _) => Channel::Bytes(reader),
        (_, Some((_, rx))) => Channel::Value(rx),
        _ => Channel::None,
    };
    Ok((handle, outgoing))
}

/// Launch every stage in `plan` in order, accumulating handles into
/// `running` and channels through the loop.  On the first launch failure,
/// drains any trailing channel and returns the error — `running`'s Drop
/// kills/reaps the stages that did spawn.  On success, returns the final
/// trailing channel (which still needs draining if it carries bytes that
/// nothing reads).
///
/// Caller owns the `PipelineGroup` so it can install the SIGINT relay
/// after launch returns; the relay's lifetime must span collect.
pub(super) fn launch_pipeline(
    stages: &[Comp],
    plan: &PipelinePlan,
    group: &mut PipelineGroup,
    running: &mut RunningPipeline,
    shell: &mut Shell,
) -> Result<Channel, EvalSignal> {
    let mut channel = Channel::None;

    for (i, stage) in stages.iter().enumerate() {
        let incoming = mem::replace(&mut channel, Channel::None);
        let ctx = LaunchContext {
            spec: &plan.specs[i],
            i,
            specs: &plan.specs,
            group,
            cancel: running.cancel_scope(),
        };
        match launch_stage(stage, ctx, shell, incoming, plan.auditing) {
            Ok((handle, outgoing)) => {
                running.add(handle);
                channel = outgoing;
            }
            Err(err) => {
                drain_trailing_bytes(&mut channel);
                return Err(err);
            }
        }
        // `claim_foreground` decides per-mode whether to acquire — see
        // its docstring.  It is idempotent and a no-op until the leader
        // pgid exists, so calling unconditionally on every iteration is
        // both correct and the cheapest place to cover both
        // `PureExternal` and `Mixed` (in `_ed-tui`) pipelines.
        group.claim_foreground(shell);
    }

    Ok(channel)
}

/// Dispatch to the appropriate launcher and return the new inter-stage channel
/// and the live handle for later collection.
pub(super) fn launch_stage(
    stage: &Comp,
    ctx: LaunchContext,
    shell: &mut Shell,
    incoming: Channel,
    auditing: bool,
) -> Result<(StageHandle, Channel), EvalSignal> {
    let is_last = ctx.i == ctx.specs.len() - 1;
    let next_input = ctx.specs.get(ctx.i + 1).map(|s| s.comp_type.input);
    let next_is_ext = ctx
        .specs
        .get(ctx.i + 1)
        .is_some_and(|s| s.dispatch.is_external());

    if ctx.spec.dispatch.is_external() {
        let (handle, outgoing) = launch_external_stage(
            ctx.spec,
            is_last,
            next_is_ext,
            shell,
            incoming,
            ctx.group,
            auditing,
        )?;
        Ok((StageHandle::Process(handle), outgoing))
    } else {
        let (handle, outgoing) = launch_internal_stage(
            stage,
            ctx.spec.comp_type,
            is_last,
            next_input,
            shell,
            incoming,
            ctx.cancel,
        )?;
        Ok((StageHandle::Thread(handle), outgoing))
    }
}

// ╔═══ Collect ═════════════════════════════════════════════════════════════╗

/// Spawn a background thread to consume any unconsumed trailing byte channel.
///
/// Streams bytes straight into `io::sink()` rather than buffering them: a
/// noisy detached producer must not be able to grow this thread's allocation
/// without bound.
pub(super) fn drain_trailing_bytes(channel: &mut Channel) {
    if let Channel::Bytes(r) = mem::replace(channel, Channel::None) {
        std::thread::spawn(move || {
            let mut r = r;
            let _ = std::io::copy(&mut r, &mut std::io::sink());
        });
    }
}

/// Determine the pipeline's return value from the final stage's result.
///
/// When the final stage produces bytes (its output goes to stdout), the
/// pipeline returns `Unit` regardless of thread result, since the bytes
/// have already been written.  Otherwise the thread's result is used directly.
fn finalize(
    last_output: crate::ty::Mode,
    last_result: Option<Result<Value, EvalSignal>>,
) -> Result<Value, EvalSignal> {
    match (last_output, last_result) {
        (crate::ty::Mode::Bytes, Some(Err(e))) => Err(e),
        (crate::ty::Mode::Bytes, _) => Ok(Value::Unit),
        (_, Some(result)) => result,
        (_, None) => Ok(Value::Unit),
    }
}

/// Accumulator for pipeline stage outcomes.
///
/// As each stage handle is joined, the collector records failures and
/// captures the last stage's structured result.  After the join loop,
/// `finish` inspects the accumulated state and returns either the
/// pipeline's value or the first failure error.
pub(super) struct PipelineCollector {
    failed: bool,
    status: i32,
    /// The first error encountered from any stage.
    error: Option<Error>,
    /// The structured result of the final stage (internal stages only).
    last_result: Option<Result<Value, EvalSignal>>,
}

impl PipelineCollector {
    fn new() -> Self {
        Self {
            failed: false,
            status: 0,
            error: None,
            last_result: None,
        }
    }

    /// Record a stage failure, keeping the first error and status encountered.
    fn note_failure(&mut self, status: i32, error: Option<Error>) {
        if !self.failed {
            self.failed = true;
            self.status = status;
            self.error = error;
        } else if self.error.is_none() {
            self.error = error;
        }
    }

    /// Join an external-process stage handle: wait for exit, record status,
    /// and note failure with a diagnostic if the exit code is non-zero.
    fn observe_process(
        &mut self,
        shell: &mut Shell,
        is_last: bool,
        handle: ProcessHandle,
    ) -> Result<(), EvalSignal> {
        let (line, col, name_len) = (handle.line, handle.col, handle.name.len());
        let name = handle.name.clone();
        let effective = handle.join(shell, is_last)?;
        if effective != 0 {
            let hint = shell.exit_hints.lookup(&name, effective);
            let mut err = Error::new(format!("{name}: exited with status {effective}"), effective)
                .at_loc(crate::diagnostic::SourceLoc {
                    file: String::new(),
                    line,
                    col,
                    len: name_len,
                });
            if let Some(h) = hint {
                err.hint = Some(h);
            }
            self.note_failure(effective, Some(err));
        }
        shell.control.last_status = effective;
        Ok(())
    }

    /// Join an internal (thread-based) stage handle.
    ///
    /// If this is the final stage, its result is saved for `finish` and its
    /// `last_status` is rejoined into the parent shell.  For non-final
    /// stages, errors are recorded as failures.
    fn observe_thread(
        &mut self,
        is_last: bool,
        shell: &mut Shell,
        handle: std::thread::JoinHandle<(Result<Value, EvalSignal>, i32)>,
    ) {
        let (result, last_status) = handle.join().unwrap_or_else(|_| {
            (
                Err(EvalSignal::Error(Error::new("pipeline stage panicked", 1))),
                1,
            )
        });
        if is_last {
            shell.control.last_status = last_status;
            self.last_result = Some(result);
        } else if let Err(EvalSignal::Error(err)) = result {
            self.note_failure(err.status, Some(err));
        } else if result.is_err() {
            self.failed = true;
        }
    }

    /// Produce the pipeline's final result.
    ///
    /// If any stage failed, returns the first recorded error.  Otherwise
    /// delegates to `finalize` to extract the value from the last stage.
    pub(super) fn finish(
        self,
        shell: &mut Shell,
        last_output: crate::ty::Mode,
    ) -> Result<Value, EvalSignal> {
        if self.failed {
            shell.control.last_status = self.status;
            let err = self.error.unwrap_or_else(|| {
                Error::new(
                    format!("pipeline exited with status {}", self.status),
                    self.status,
                )
                .at(shell.location.line, shell.location.col)
            });
            return Err(EvalSignal::Error(err));
        }
        finalize(last_output, self.last_result)
    }
}

/// Owns every spawned child and evaluator thread for the duration of a
/// pipeline run, plus the cancel scope that ties them together.
///
/// Each `ProcessHandle` carries its own destructor — kill pgid + reap on
/// the abort path; no-op after a successful `join`.  Internal stage
/// threads inherit `cancel` at spawn time and observe it at every
/// `signal::check` poll point — so `Drop`'s abort path can guarantee
/// they actually exit by setting the flag *before* joining their handles.
///
/// Drop order on abort: cancel the scope (so threads will unwind at
/// their next poll), then drop handles.  `ProcessHandle`s SIGKILL the
/// pgid via their own `Drop`; thread handles are joined explicitly so
/// abort waits for ral code to actually exit rather than detaching it.
///
/// `collect` consumes `self.handles` via `mem::take`, leaving it empty;
/// `Drop` then sees an empty Vec and skips both the cancel and the
/// thread-join loop — those would either be no-ops (threads already
/// finished) or actively wrong (cancelling the scope right before it's
/// dropped is harmless, but cleaner not to).
pub(super) struct RunningPipeline {
    handles: Vec<StageHandle>,
    cancel: crate::signal::CancelScope,
}

impl RunningPipeline {
    pub(super) fn new(cancel: crate::signal::CancelScope) -> Self {
        Self {
            handles: Vec::new(),
            cancel,
        }
    }

    pub(super) fn add(&mut self, handle: StageHandle) {
        self.handles.push(handle);
    }

    /// The scope to stamp on each spawned thread's `Shell.cancel`.  All
    /// threads in this pipeline share the same flag, so a single
    /// `cancel.cancel()` unwinds every one of them.
    pub(super) fn cancel_scope(&self) -> crate::signal::CancelScope {
        self.cancel.clone()
    }

    /// Success path: join every handle in stage order and return the
    /// accumulated outcomes.  Even if a `join` panics partway through,
    /// the local `handles` Vec drops, taking remaining
    /// `ProcessHandle`s with it (their `Drop` SIGKILLs the pgid).  Any
    /// remaining thread handles in that scenario detach by default —
    /// the same gap that the abort path's explicit join + cancel
    /// closes; collect-time panics are rare enough to accept it.
    pub(super) fn collect(mut self, shell: &mut Shell, stage_count: usize) -> PipelineCollector {
        let handles = mem::take(&mut self.handles);
        let mut collector = PipelineCollector::new();
        let last = stage_count.saturating_sub(1);
        for (idx, handle) in handles.into_iter().enumerate() {
            let is_last = idx == last;
            match handle {
                StageHandle::Process(ph) => {
                    if let Err(EvalSignal::Error(err)) =
                        collector.observe_process(shell, is_last, ph)
                    {
                        collector.note_failure(err.status, Some(err));
                    }
                }
                StageHandle::Thread(jh) => collector.observe_thread(is_last, shell, jh),
            }
        }
        collector
    }
}

impl Drop for RunningPipeline {
    /// Abort-path cleanup: cancel the scope, then drain handles.
    ///
    /// Cancelling first means any internal stage threads still running
    /// ral code will observe the cancel at their next `signal::check`
    /// and unwind via `EvalSignal::Error("cancelled")` — so the
    /// subsequent `jh.join()` actually returns rather than waiting
    /// forever for a CPU loop with no I/O.
    fn drop(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        self.cancel.cancel();
        for handle in self.handles.drain(..) {
            match handle {
                StageHandle::Process(_ph) => {
                    // ProcessHandle::Drop runs at end of arm scope.
                }
                StageHandle::Thread(jh) => {
                    let _ = jh.join();
                }
            }
        }
    }
}
