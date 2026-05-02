//! Pipeline execution engine.
//!
//! Orchestrates multi-stage pipelines through three explicit phases:
//!
//!   1. **resolve** ([`stages::resolve_pipeline`]): type-check every stage,
//!      classify dispatch (External vs Internal), eagerly evaluate argv,
//!      and freeze the pipeline-level invariants (mode, last-output, audit).
//!   2. **launch** ([`stages::launch_pipeline`]): walk stages once, choosing
//!      per stage between sync invocation (value-only internals) and
//!      threaded execution (anything that touches bytes).  Threaded
//!      stages with value output are joined immediately so their value
//!      can feed the next stage; everything else stays in
//!      `RunningPipeline` for the collector.
//!   3. **collect** (`RunningPipeline::collect` + `PipelineCollector::finish`):
//!      wait for the remaining byte-output stages, surface the first
//!      error, return the pipeline's final value (already known from
//!      launch) or `Unit` for byte-output endings.
//!
//! `run_pipeline` is the few-line orchestrator; nothing more.

mod group;
mod stages;

use crate::ir::Comp;
use crate::types::*;

use group::PipelineGroup;
use stages::{RunningPipeline, drain_trailing_bytes, launch_pipeline, resolve_pipeline};

/// True for an exit code that means "downstream consumer closed the pipe
/// before this stage was done writing" — not a failure for a non-final
/// stage.
///
/// Two conventions in play:
///   * Unix: `128 + SIGPIPE` = 141.  Set by `reset_child_signals` so a
///     write to a closed pipe terminates the child via SIGPIPE.
///   * Windows: `STATUS_PIPE_BROKEN` = 0xC000_00B1, surfaced as the exit
///     code of a process that wrote to a closed pipe and was unwound by
///     the kernel.  Sign-extended to i32 by `ExitStatus::code`.
fn is_broken_pipe_exit(code: i32) -> bool {
    code == 141 || (code as u32) == 0xC000_00B1
}

/// Execute a multi-stage pipeline: resolve, launch, collect.
///
/// Pure-value-internal pipelines reduce to a sequential fold over
/// `invoke::invoke` — no threads ever spawn, the byte-pipe accumulator
/// stays empty, `running` is empty.  The same launch loop, walking the
/// same plan, handles mixed and pure-byte pipelines by promoting
/// byte-touching stages to threads/processes connected by OS pipes.
/// Process foreground is RAII-managed by `PipelineGroup`; the SIGINT
/// relay's lifetime spans collect so a Ctrl-C during wait reaches every
/// child.
pub(crate) fn run_pipeline(stages: &[Comp], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let plan = resolve_pipeline(stages, shell)?;

    let mut group = PipelineGroup::new(plan.mode);
    // Pipeline-scoped cancel: a fresh scope under the shell's current
    // scope, so cancelling this pipeline does *not* propagate up to the
    // parent shell — but propagation downward into stage threads
    // (which inherit it via launch_internal_stage) means
    // `RunningPipeline::Drop` can unwind everything by setting the flag.
    let mut running = RunningPipeline::new(shell.cancel.child());

    let outcome = launch_pipeline(stages, &plan, &mut group, &mut running, shell)?;

    let _relay = group.install_relay();
    let mut trailing = outcome.trailing_byte_pipe;
    drain_trailing_bytes(&mut trailing);

    // The pipeline's final stage is "in running" only when its handle
    // is actually there — i.e. when launch did *not* already produce
    // the final value (sync stage or early-joined value-output thread).
    let pipeline_final_in_running = outcome.final_value.is_none() && !running.is_empty();

    running
        .collect(shell, pipeline_final_in_running)
        .finish(shell, plan.last_output, outcome.final_value)
}
