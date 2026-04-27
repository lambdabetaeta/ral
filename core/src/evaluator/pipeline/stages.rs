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

use std::io::Read;
use std::mem;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use super::group::PipelineGroup;
use crate::io::{Sink, Source};
use crate::ir::{Comp, CompKind, ExecName, Val};
use crate::ty::InferCtx;
use crate::types::*;

// ╔═══ Analysis ════════════════════════════════════════════════════════════╗

/// Resolved name and pre-evaluated arguments for an external command stage.
#[derive(Clone, Debug)]
pub(super) struct ExternalStage {
    pub(super) name: ExecName,
    pub(super) args: Vec<String>,
}

/// Per-stage analysis result: resolved comp type, optional external-command
/// info, and source position.
#[derive(Clone, Debug)]
pub(super) struct StageSpec {
    pub(super) comp_type: crate::ty::CompType,
    pub(super) external: Option<ExternalStage>,
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

/// Analyze a single pipeline stage.
///
/// Resolves the head name against builtins and the environment to determine
/// whether the stage is external or internal, infers its channel types, and
/// (for external stages) eagerly evaluates its arguments.
fn analyze_stage(stage: &Comp, shell: &mut Shell, ctx: &mut InferCtx) -> Result<StageSpec, EvalSignal> {
    let (line, col) = stage_position(stage, shell);
    let Some(name) = stage_head_name(stage) else {
        return Ok(StageSpec {
            comp_type: ctx.comp_type(stage, None),
            external: None,
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

    let external = match (&stage.kind, name) {
        (CompKind::Exec { args, .. }, StageHead::Exec(name)) if !hit.internal => {
            let vals = super::super::dispatch::eval_call_args(args, None, shell)?;
            Some(ExternalStage {
                name: name.clone(),
                args: vals.iter().map(|v| v.to_string()).collect(),
            })
        }
        _ => None,
    };

    Ok(StageSpec {
        comp_type: hit.comp_type,
        external,
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

/// Analyze all pipeline stages: infer types, validate adjacency, resolve mode
/// variables (defaulting unconstrained vars to `None`).
pub(super) fn analyze_stages(
    stages: &[Comp],
    shell: &mut Shell,
) -> Result<Vec<StageSpec>, EvalSignal> {
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
    Ok(specs)
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
pub(super) struct ProcessHandle {
    name: String,
    args: Vec<String>,
    line: usize,
    col: usize,
    child: std::process::Child,
    /// Pump thread draining piped stdout into the next stage or final sink.
    /// `None` when stdout is inherited or connected via a direct OS pipe.
    pump: Option<std::thread::JoinHandle<()>>,
    /// Capture buffer for auditing; written by the pump's Tee sink.
    audit_capture: Option<Arc<Mutex<Vec<u8>>>>,
    /// Wall-clock start time (µs since epoch); 0 when not auditing.
    start_us: i64,
}

impl ProcessHandle {
    /// Join pump, drain stderr, wait for child, record audit node.
    ///
    /// The pump must be joined before `child.wait()` to drop its pipe-writer
    /// end, preventing a deadlock when the pipe reader feeds the next stage.
    /// After the pump is joined, stdout is fully consumed; stderr can be read
    /// synchronously without risk of blocking.
    fn join(mut self, shell: &mut Shell, is_last: bool) -> Result<i32, EvalSignal> {
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }

        let mut stderr = Vec::new();
        if let Some(mut s) = self.child.stderr.take() {
            // Cap captured head; drain the rest so the child can't stall.
            let _ = s.by_ref().take(65536).read_to_end(&mut stderr);
            let _ = std::io::copy(&mut s, &mut std::io::sink());
        }
        if !stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&stderr));
        }

        let status = self
            .child
            .wait()
            .map_err(|e| EvalSignal::Error(Error::new(format!("{}: {e}", self.name), 1)))?;
        let code = super::super::exec::exit_code(status);
        let effective = if !is_last && code == super::SIGPIPE_STATUS {
            0
        } else {
            code
        };

        // Strip one trailing newline from captured stdout — same as $().
        let mut stdout = self
            .audit_capture
            .and_then(|b| b.lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        if stdout.last() == Some(&b'\n') {
            stdout.pop();
        }

        if let Some(tree) = &mut shell.audit.tree {
            let mut node = ExecNode::leaf(
                &self.name,
                self.args,
                effective,
                &shell.location.call_site.script,
                self.line,
                self.col,
            );
            node.stdout = stdout;
            node.stderr = stderr;
            node.start = self.start_us;
            node.end = epoch_us();
            node.principal = shell
                .dynamic
                .env_vars
                .get("USER")
                .cloned()
                .unwrap_or_default();
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
    let external = spec
        .external
        .clone()
        .expect("launch_external_stage called for internal stage");
    let stdout_plan = plan_stdout(
        is_last,
        next_is_ext,
        shell.io.terminal.stdout_tty && matches!(shell.io.stdout, Sink::Terminal | Sink::External(_)),
        auditing,
    );

    let shown = super::super::exec::render_exec_name(&external.name, shell);
    let resolved = super::super::exec::resolve_in_path(&external.name, shell);
    let policy_names = super::super::exec::exec_policy_names(&external.name, shell, &resolved);
    let policy_name_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    shell.check_exec_args(&shown, &policy_name_refs, &external.args)?;
    let mut cmd = crate::sandbox::make_command(&resolved, &external.args, shell);
    super::super::exec::apply_env(&mut cmd, shell);
    cmd.stderr(if auditing {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    // External stages consume only a byte stream from their predecessor.
    // Value channels are dropped, unblocking the sender.
    cmd.stdin(match incoming {
        Channel::Bytes(r) => Stdio::from(r),
        _ => Stdio::inherit(),
    });

    let mut outgoing_reader: Option<os_pipe::PipeReader> = None;

    match stdout_plan {
        // Preserve the terminal for the final uncaptured stage so the child can
        // detect TTY stdout and pick colour/pager behavior itself.
        StdoutPlan::Inherit => {
            cmd.stdout(Stdio::inherit());
        }
        // Adjacent external stages can be connected with a plain OS pipe.
        StdoutPlan::DirectPipe => {
            let (r, w) = create_pipe()?;
            cmd.stdout(Stdio::from(w));
            outgoing_reader = Some(r);
        }
        // Any topology involving shell interception uses a piped stdout and
        // a pump thread after spawn.
        StdoutPlan::Pump => {
            cmd.stdout(Stdio::piped());
        }
    };

    group.pre_exec_hook(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| super::super::exec::spawn_error(&shown, e))?;
    group.register_child(&child);
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }

    // Build the pump sink when stdout is piped.
    //
    // Last stage  → shell.io.stdout (capture buffer under let-binding, else terminal).
    // Non-last    → fresh OS pipe; reader feeds the next stage's stdin.
    // Auditing    → wrap in Sink::Tee so bytes flow to the real destination and
    //               also accumulate in audit_capture for the exec tree.
    let mut audit_capture: Option<Arc<Mutex<Vec<u8>>>> = None;
    let pump = match stdout_plan {
        StdoutPlan::Inherit | StdoutPlan::DirectPipe => None,
        StdoutPlan::Pump => {
            let child_stdout = child.stdout.take().ok_or_else(|| {
                EvalSignal::Error(Error::new("pipeline: missing child stdout", 1))
            })?;
            let base_sink = if is_last {
                shell.io.stdout.try_clone().map_err(pipe_error)?
            } else {
                let (reader, writer) = create_pipe()?;
                outgoing_reader = Some(reader);
                Sink::Pipe(writer)
            };
            let sink = if auditing {
                let cap = Arc::new(Mutex::new(Vec::<u8>::new()));
                audit_capture = Some(cap.clone());
                Sink::Tee(Box::new(Sink::Buffer(cap)), Box::new(base_sink))
            } else {
                base_sink
            };
            Some(sink.pump(child_stdout))
        }
    };

    let handle = ProcessHandle {
        name: shown,
        args: external.args,
        line: spec.line,
        col: spec.col,
        child,
        pump,
        audit_capture,
        start_us: if auditing { epoch_us() } else { 0 },
    };

    Ok((
        handle,
        outgoing_reader.map_or(Channel::None, Channel::Bytes),
    ))
}

fn launch_internal_stage(
    stage: &Comp,
    comp_type: crate::ty::CompType,
    is_last: bool,
    next_input: Option<crate::ty::Mode>,
    shell: &Shell,
    incoming: Channel,
) -> Result<(std::thread::JoinHandle<(Result<Value, EvalSignal>, i32)>, Channel), EvalSignal> {
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

        let result = super::super::eval_comp(&stage_comp, child_env);
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
        .is_some_and(|s| s.external.is_some());

    if ctx.spec.external.is_some() {
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
            ctx.spec.comp_type.clone(),
            is_last,
            next_input,
            shell,
            incoming,
        )?;
        Ok((StageHandle::Thread(handle), outgoing))
    }
}

// ╔═══ Collect ═════════════════════════════════════════════════════════════╗

/// Spawn a background thread to consume any unconsumed trailing byte channel.
pub(super) fn drain_trailing_bytes(channel: &mut Channel) {
    if let Channel::Bytes(r) = mem::replace(channel, Channel::None) {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = (&r).read_to_end(&mut b);
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

/// Join all stage handles in order, accumulating outcomes into a
/// [`PipelineCollector`].
///
/// Every handle is awaited even if an earlier one errors — bailing
/// midway would orphan still-running children and pump threads.
pub(super) fn collect_handles(
    handles: Vec<StageHandle>,
    shell: &mut Shell,
    stage_count: usize,
) -> PipelineCollector {
    let mut collector = PipelineCollector::new();
    let last = stage_count.saturating_sub(1);
    for (idx, handle) in handles.into_iter().enumerate() {
        let is_last = idx == last;
        match handle {
            StageHandle::Process(ph) => {
                if let Err(EvalSignal::Error(err)) = collector.observe_process(shell, is_last, ph) {
                    collector.note_failure(err.status, Some(err));
                }
            }
            StageHandle::Thread(jh) => collector.observe_thread(is_last, shell, jh),
        }
    }
    collector
}
