//! Pipeline execution engine.
//!
//! Orchestrates multi-stage pipelines through three explicit phases:
//!
//!   1. **resolve** ([`stages::resolve_pipeline`]): type-check every stage,
//!      classify dispatch (External vs Internal), eagerly evaluate argv,
//!      and freeze the pipeline-level invariants (mode, last-output, audit).
//!   2. **launch** ([`stages::launch_pipeline`]): spawn each stage in order,
//!      wiring inter-stage channels.  Every spawned child is owned by a
//!      `RunningPipeline` whose `Drop` kills it on early return.
//!   3. **collect** (`RunningPipeline::collect` + `PipelineCollector::finish`):
//!      wait for every child / thread, accumulate exit statuses, return the
//!      final value or first error.
//!
//! `run_pipeline` is the four-line orchestrator; nothing more.

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
/// Returns the value produced by the final stage, or an error if any
/// non-final stage exits with a non-zero, non-SIGPIPE status.  Process
/// foreground is RAII-managed by `PipelineGroup`; the SIGINT relay's
/// lifetime spans collect so a Ctrl-C during wait reaches every child.
pub(crate) fn run_pipeline(stages: &[Comp], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let plan = resolve_pipeline(stages, shell)?;
    let mut group = PipelineGroup::new(plan.mode);
    // Pipeline-scoped cancel: a fresh scope under the shell's current
    // scope, so cancelling this pipeline does *not* propagate up to the
    // parent shell — but propagation downward into stage threads
    // (which inherit it via launch_internal_stage) means
    // `RunningPipeline::Drop` can unwind everything by setting the flag.
    let mut running = RunningPipeline::new(shell.cancel.child());

    let mut trailing = launch_pipeline(stages, &plan, &mut group, &mut running, shell)?;

    let _relay = group.install_relay();
    drain_trailing_bytes(&mut trailing);

    running
        .collect(shell, stages.len())
        .finish(shell, plan.last_output)
}
