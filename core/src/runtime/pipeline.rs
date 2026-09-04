//! Pipeline execution engine.  `resolve` freezes the whole plan — the form's
//! yield and each stage's launch decision; `launch` walks the stages once,
//! placing every one in a single process group; `join` (`collect` then
//! `finish`) waits in launch order, surfaces the first error, and recovers
//! the final value.  [`PipeNode`] is the orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
pub(crate) mod resolve;
mod route;
mod thread;

use crate::ir::{Comp, PipeYield};
use crate::types::{Env, Error, Mooring, Settled, Shell, Value};
use std::sync::Arc;
use std::time::Instant;

use collect::CollectState;
use group::PipelineGroup;
use launch::launch_pipeline;
use resolve::resolve_pipeline;

/// A multi-stage pipeline between two launches and one join: the node the
/// `Pipeline` rule launches and joins while its stages run in their own
/// process group (§5 of the CEK plan). `group` — the pgid anchor, foreground
/// guard and SIGINT relay — stays alive across both `collect` and `finish`,
/// so it lives here rather than as a local dropped early.
///
/// Field order is teardown order, as in `PipelineResources`: an unwind
/// through the `Pipeline` rule must join the stages before the anchor.
pub(crate) struct PipeNode {
    /// Before `group`: a collector dropped mid-flight kills the pgid, which
    /// must still be the anchor's — a reaped leader's pid may be reused once
    /// its group is empty.
    collect: CollectState,
    group: PipelineGroup,
    yields: PipeYield,
}

impl PipeNode {
    /// Resolve the plan and spawn every stage into one process group —
    /// everything up to and including the spawn, no stage observed yet.
    ///
    /// A ral-written stage runs on its own thread; an external stage is a
    /// process. The final stage's value is taken from its `JoinHandle` or
    /// its wait status when the form yields it. No stage runs in the
    /// parent, so none can be in tail position.
    ///
    /// `env` is the pipeline node's own lexical environment — the machine's
    /// `E` in focus, not necessarily `shell.env` (a nested machine, inside a
    /// lambda body say, runs under its own frame env) — and is what a stage
    /// thread's closure captures. A stage's own stack is empty by
    /// construction: no frame crosses into it, only the closure.
    pub(crate) fn launch(
        stages: &[Arc<Comp>],
        yields: PipeYield,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        // The first signal-checked seam, since a top-level pipeline sits under
        // no Bind.  An earlier SIGINT cancelled the foreground scope
        // but claimed no relay pgid, so without this the pipeline would launch
        // anyway and collect would block on a child that never saw the signal.
        crate::process::check(mooring)?;
        let plan = resolve_pipeline(stages, yields, env, mooring, shell)?;

        // Window start for the sandbox-denial reader, anchored before any stage
        // spawns so a kernel deny logged by a stage falls inside it.
        let started = Instant::now();

        // Whether this pipeline owns its pgid or joins an enclosing stage's.
        let group = match shell.io.launch_role.stage_group() {
            Some(g) => PipelineGroup::joining(g),
            None => PipelineGroup::prepare(shell)?,
        };

        let (group, collect) = launch_pipeline(stages, &plan, env, mooring, shell, group, started)?;
        Ok(Self {
            collect,
            group,
            yields: plan.yields,
        })
    }

    /// Wait on every stage and fold the outcome into one value.
    ///
    /// The last stage carries its value home directly, on the `JoinHandle` or
    /// the OS wait alike; collect reads it only after waiting on the stage.
    pub(crate) fn join(mut self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        self.collect.drive(&self.group);
        self.collect.fold(mooring, shell).finish(self.yields)
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
