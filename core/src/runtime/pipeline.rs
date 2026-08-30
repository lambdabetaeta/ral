//! Pipeline execution engine.  `resolve` freezes the whole plan — the form's
//! yield and each stage's launch decision; `launch` walks the stages once,
//! placing every one in a single process group; `join` (`collect` then
//! `finish`) waits in launch order, surfaces the first error, and recovers
//! the final value.  [`PipeNode`] is the orchestrator; nothing more.

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
use std::time::Instant;

use collect::Running;
use group::PipelineGroup;
use launch::launch_pipeline;
use resolve::resolve_pipeline;

/// A multi-stage pipeline between two launches and one join: the node the
/// `Pipeline` rule launches and joins while its stages run in their own
/// process group (§5 of the CEK plan). `group` — the pgid anchor, foreground
/// guard and SIGINT relay — stays alive across both `collect` and `finish`,
/// so it lives here rather than as a local dropped early.
pub(crate) struct PipeNode {
    group: PipelineGroup,
    running: Running,
    yields: PipeYield,
    started: Instant,
}

impl PipeNode {
    /// Resolve the plan and spawn every stage into one process group —
    /// everything up to and including the spawn, no stage observed yet.
    ///
    /// Every multi-stage pipeline is process-staged.  The final helper
    /// reports its value over the response frame when the form yields it.
    /// No stage runs in the parent, so none can be in tail position.
    ///
    /// `env` is the pipeline node's own lexical environment — the machine's
    /// `E` in focus, not necessarily `shell.env` (a nested machine, inside a
    /// lambda body say, runs under its own frame env) — and is what a helper
    /// stage's closure captures. No frame ever crosses the wire to a stage:
    /// a stage's stack is empty by construction, so only `⟨comp, scrubbed
    /// E⟩` and the wire context ride along.
    pub(crate) fn launch(
        stages: &[Arc<Comp>],
        yields: PipeYield,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        // The first signal-checked seam, since a top-level pipeline sits under
        // no Bind/Chain/Seq.  An earlier SIGINT cancelled the foreground scope
        // but claimed no relay pgid, so without this the pipeline would launch
        // anyway and collect would block on a child that never saw the signal.
        crate::process::check(mooring)?;
        let plan = resolve_pipeline(stages, yields, env, mooring, shell)?;

        // Window start for the sandbox-denial reader, anchored before any stage
        // spawns so a kernel deny logged by a stage falls inside it.
        let started = Instant::now();

        let (group, running) = launch_pipeline(stages, &plan, env, mooring, shell)?;
        Ok(Self { group, running, yields: plan.yields, started })
    }

    /// Wait on every stage and fold the outcome into one value.
    ///
    /// The last helper carries its value home in its `ChildEvalResponse`
    /// frame; collect reads it only after waiting on the helper, since one
    /// blocked on a stopped upstream would deadlock us. `group` — bound here
    /// so it outlives both calls — drops with `self` once `finish` returns.
    pub(crate) fn join(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let Self { group: _group, running, yields, started } = self;
        running.collect(mooring, shell, started).finish(yields)
    }
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
