//! Pipeline execution engine.  `resolve` freezes the whole plan — kind,
//! modes, last-output, and each stage's launch decision; `launch` walks the
//! stages once, placing every one in a single process group; `collect` waits
//! in launch order, surfaces the first error, and recovers the final value.
//! `run_pipeline` is the orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
mod protocol;
pub(crate) mod resolve;
mod route;
mod stage;

use crate::ir::Comp;
use crate::types::{Error, Mooring, Raw, Settled, Shell, Tail, Value};
use std::sync::Arc;

use launch::launch_pipeline;
use resolve::{PipelineKind, resolve_pipeline};

/// Execute a multi-stage pipeline: resolve, launch, collect.
///
/// A `PureValue` pipeline has no byte edge, so it folds in the parent
/// evaluator and enters no job-control machinery at all.  `tail` reaches
/// only that fold, which grants it to the final stage; the process-staged
/// path collects its value over the wire and emits no tail call.
pub(crate) fn run_pipeline(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    // The first signal-checked seam, since a top-level pipeline sits under
    // no Bind/Chain/Seq.  An earlier SIGINT cancelled the foreground scope
    // but claimed no relay pgid, so without this the pipeline would launch
    // anyway and collect would block on a child that never saw the signal.
    crate::process::check(mooring)?;
    let plan = resolve_pipeline(stages, wires, mooring, shell)?;
    if plan.kind == PipelineKind::PureValue {
        return run_value_fold(stages, tail, mooring, shell);
    }

    // Window start for the sandbox-denial reader, anchored before any stage
    // spawns so a kernel deny logged by a stage falls inside it.
    let started = std::time::Instant::now();

    // `_group` keeps the anchor, foreground guard, and SIGINT relay alive to
    // end of scope, so they outlive `finish`.
    let (_group, running) = launch_pipeline(stages, &plan, mooring, shell)?;

    // The last value-typed helper carries its value home in its
    // `ChildEvalResponse` frame; collect reads it only after waiting on the
    // helper, since one blocked on a stopped upstream would deadlock us.
    running
        .collect(shell, started)
        .finish(shell, plan.last_output)
        .map_err(Into::into)
}

/// Attach a kernel-denial diagnostic to a failed pipeline stage's error.
///
/// Attribution is best-effort: sibling stages share the pipeline group and
/// may still be alive, so collect holds no exact per-stage PID and the reader
/// scopes deny lines to a descendant sample of this process taken now.
/// `started` is the pipeline-wide window start.
fn augment_stage_failure(err: Error, shell: &Shell, started: std::time::Instant) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let pids = crate::sandbox::sample_descendants(std::process::id());
    crate::sandbox::augment_failure(err, shell, &pids, started)
}

/// Sequential data-last fold for pure-value pipelines: each stage takes the
/// previous stage's value as its final argument, `x | f == f !{x}`.
///
/// Only producers are forced as their value crosses the edge; the last
/// stage's value is the pipeline's own result and is returned as evaluated.
/// Only that stage inherits the pipeline's tail position — an earlier
/// stage's tail call would escape as a `TailCall` discarding every stage
/// downstream of it.
fn run_value_fold(
    stages: &[Arc<Comp>],
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let mut acc: Option<Value> = None;
    let last = stages.len() - 1;
    for (i, stage) in stages.iter().enumerate() {
        let stage_tail = if i == last { tail } else { Tail::No };
        let value = crate::evaluator::call::invoke(stage, acc.take(), stage_tail, mooring, shell)?;
        let value = if i < last {
            force_pipe_value(value, mooring, shell)?
        } else {
            value
        };
        acc = Some(value);
    }
    Ok(acc.unwrap_or(Value::Unit))
}

/// The runtime `!{x}` at a pipeline value edge: a suspended
/// [`Value::Block`] runs and yields its body's value; every other value —
/// including a lambda, whose force is the identity — passes through.
///
/// This single force is the value-level twin of the checker's
/// `deref_forced_producer`, which derefs one thunk level at a value edge.
/// Both the fold above and the re-exec stage child (`child_eval`) call
/// here, so the two cannot drift.
pub(crate) fn force_pipe_value(
    value: Value,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    match value {
        // The value still has to cross into the next stage — a non-trivial
        // continuation, so `Tail::No`.
        Value::Block { body, captured } => {
            crate::evaluator::eval_block(&body, &captured, Tail::No, mooring, shell)
        }
        other => Ok(other),
    }
}
