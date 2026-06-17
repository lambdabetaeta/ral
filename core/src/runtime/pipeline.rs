//! Pipeline execution engine.
//!
//! Orchestrates multi-stage pipelines through three explicit phases:
//!
//!   1. **resolve** ([`resolve::resolve_pipeline`]): type-check every stage,
//!      classify dispatch, eagerly evaluate argv, and freeze the
//!      pipeline-level invariants (kind, mode, last-output). The
//!      byte-capturing audit decision is consulted live during launch
//!      classification rather than stored on the plan.
//!   2. **launch** ([`launch::launch_pipeline`]): walk stages once,
//!      spawning every process-staged stage as a ral helper subprocess
//!      in one process group.
//!   3. **collect** (`collect::RunningPipeline::collect` +
//!      `PipelineCollector::finish`): wait for the processes in launch
//!      order, surface the first error, and recover the final value when
//!      the last stage is value-typed.
//!
//! `run_pipeline` is the few-line orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
mod protocol;
mod resolve;
mod route;
mod stage;

use crate::ir::Comp;
use crate::types::*;
use std::sync::Arc;

use launch::launch_pipeline;
use resolve::{PipelineKind, resolve_pipeline};

/// Execute a multi-stage pipeline: resolve, launch, collect.
///
/// `PipelineKind::PureValue` reduces to a sequential fold over
/// `call::invoke`: `x | f` becomes `f !{x}` in the parent evaluator.
/// No threads spawn, no byte pipes exist, and no job-control machinery
/// is entered. `PipelineKind::ProcessStaged` launches every byte-capable
/// stage as a subprocess in one pipeline group.
///
/// `tail` is the pipeline's tail position. It reaches the value fold,
/// which grants it to the final stage alone; the process-staged path
/// collects a value over the wire and emits no tail call, so `tail` has
/// no effect there.
pub(crate) fn run_pipeline(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    tail: Tail,
    shell: &mut Shell,
) -> Raw<Value> {
    // A SIGINT delivered before the first signal-checked seam (a
    // top-level pipeline has no outer Bind/Chain/Seq) would otherwise
    // be silently consumed: the handler increments SIGNAL_COUNT but
    // RELAY_PGIDS is empty, so the pipeline launches anyway and
    // collect blocks on a long-running consumer that never received
    // the signal.  Bail here instead.
    crate::process::check(shell)?;
    let plan = resolve_pipeline(stages, wires, shell)?;
    if plan.kind == PipelineKind::PureValue {
        return run_value_fold(stages, tail, shell);
    }

    // The pipeline group's SIGINT-forwarding relay slot is claimed
    // inside `PipelineGroup::spawn` once the first real child has
    // joined the pgid (see `group.rs`'s SIGINT/relay invariant).
    // Earlier SIGINTs only increment `SIGNAL_COUNT`, which the per-
    // stage `signal::check` inside `launch_pipeline` reads to abort
    // promptly.
    let (_group, running) = launch_pipeline(stages, &plan, shell)?;

    // The last value-typed helper carries its own value back inside
    // its `ChildEvalResponse` frame; collect recovers the value after waiting
    // on the helper, never before — a helper blocked on a stopped
    // upstream would otherwise deadlock the parent here.  `_group`
    // lives until end-of-scope, tearing the anchor / foreground guard /
    // SIGINT relay down only after `finish` has returned.
    //
    // The process-staged pipeline cannot emit a [`Tail`] — every stage
    // ran in its own helper subprocess — so `finish`'s `Settled` widens
    // losslessly into the evaluator's `Raw` carrier via `Into`.
    running
        .collect(shell)
        .finish(shell, plan.last_output)
        .map_err(Into::into)
}

/// Sequential data-last fold for pure-value pipelines.
///
/// Each stage receives the previous stage's value as its final argument
/// via [`crate::evaluator::call::invoke`]: `x | f == f !{x}`,
/// unconditionally.
///
/// A producer stage that is a bare block (`{ … }` with no upstream to
/// apply) evaluates to a [`Value::Block`] thunk rather than running its
/// body — `invoke`'s fall-through arm returns the thunk unforced.  But
/// the checker models a value-producing stage feeding a value consumer
/// by piping the producer's *return* type (see `infer_pipeline`'s
/// `extract_return` at the value edge), i.e. the body's result, not the
/// thunk.  Mirror that here: force a producer's block result once
/// before it crosses to the next stage, so `{ fail "x" } | { |v| … }`
/// runs the producer body (raising) instead of handing the consumer a
/// phantom `<block>` value.  The single force mirrors the checker's
/// single thunk deref; whatever the body itself returns is not forced
/// recursively.  Only producers are forced; the final stage's value is
/// the pipeline's own result and is returned as evaluated.
///
/// Only the final stage inherits the pipeline's tail position. A
/// non-final stage runs under a non-trivial continuation
/// ([`Tail::No`]): its value must cross the value edge into the next
/// stage, so its tail call must not escape as a [`TailCall`] that
/// discards every downstream stage.
fn run_value_fold(stages: &[Arc<Comp>], tail: Tail, shell: &mut Shell) -> Raw<Value> {
    let mut acc: Option<Value> = None;
    let last = stages.len() - 1;
    for (i, stage) in stages.iter().enumerate() {
        let stage_tail = if i == last { tail } else { Tail::No };
        let value = crate::evaluator::call::invoke(stage, acc.take(), stage_tail, shell)?;
        // `x | f = f !{x}`: a non-final stage's value crosses a value
        // edge, so it is forced once before the next stage consumes it.
        // The final stage's value is the pipeline's own result, returned
        // as evaluated.
        let value = if i < last {
            force_pipe_value(value, shell)?
        } else {
            value
        };
        acc = Some(value);
    }
    Ok(acc.unwrap_or(Value::Unit))
}

/// The runtime `!{x}` at a pipeline value edge.
///
/// `x | f = f !{x}`: when a producer's value crosses a value edge to its
/// consumer it is forced exactly once.  A suspended block
/// ([`Value::Block`]) runs and yields its body's value; every other value
/// — a concrete value, or a lambda, whose force is the identity — passes
/// through.  This is the sole value-level realisation of the checker's
/// `deref_forced_producer`: both the pure-value fold above and the
/// process-staged stage's re-exec child
/// ([`run_child_eval`](crate::child_eval::run_child_eval)) call it, so the
/// two cannot drift.
pub(crate) fn force_pipe_value(value: Value, shell: &mut Shell) -> Settled<Value> {
    match value {
        // The forced producer's value crosses the value edge into the
        // next stage — a non-trivial continuation, so [`Tail::No`].
        Value::Block { body, captured } => {
            crate::evaluator::eval_block(&body, captured, Tail::No, shell)
        }
        other => Ok(other),
    }
}
