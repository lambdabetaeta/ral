//! Pipeline execution engine.  `resolve` freezes the whole plan — the form's
//! yield and each stage's launch decision; `launch` walks the stages once,
//! placing every one in a single process group; `collect` waits in launch
//! order, surfaces the first error, and recovers the final value.
//! `run_pipeline` is the orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
mod protocol;
pub(crate) mod resolve;
mod route;
mod stage;

use crate::ir::{Comp, PipeYield};
use crate::types::{Env, Error, Mooring, Settled, Shell, Value};
use std::sync::Arc;

use launch::launch_pipeline;
use resolve::resolve_pipeline;

/// Execute a multi-stage pipeline: resolve, launch, collect.
///
/// Every multi-stage pipeline is process-staged.  The final helper reports
/// its value over the response frame when the form yields it.  No stage runs
/// in the parent, so none can be in tail position.
///
/// `env` is the pipeline node's own lexical environment — the machine's `E`
/// in focus, not necessarily `shell.env` (a nested machine, inside a lambda
/// body say, runs under its own frame env) — and is what a helper stage's
/// closure captures.
pub(crate) fn run_pipeline(
    stages: &[Arc<Comp>],
    yields: PipeYield,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    // The first signal-checked seam, since a top-level pipeline sits under
    // no Bind/Chain/Seq.  An earlier SIGINT cancelled the foreground scope
    // but claimed no relay pgid, so without this the pipeline would launch
    // anyway and collect would block on a child that never saw the signal.
    crate::process::check(mooring)?;
    let plan = resolve_pipeline(stages, yields, env, mooring, shell)?;

    // Window start for the sandbox-denial reader, anchored before any stage
    // spawns so a kernel deny logged by a stage falls inside it.
    let started = std::time::Instant::now();

    // `_group` keeps the anchor, foreground guard, and SIGINT relay alive to
    // end of scope, so they outlive `finish`.
    let (_group, running) = launch_pipeline(stages, &plan, env, mooring, shell)?;

    // The last helper carries its value home in its `ChildEvalResponse`
    // frame; collect reads it only after waiting on the helper, since one
    // blocked on a stopped upstream would deadlock us.
    running.collect(mooring, shell, started).finish(shell, plan.yields)
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
