//! Pipeline execution engine.  `resolve` freezes the terminal handoff and
//! each stage's launch decision, `launch` places every stage in one process
//! group, `join` folds their observations into one value.  [`PipeNode`] is
//! the orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
pub(crate) mod resolve;
mod route;
mod sentinel;
mod thread;

use crate::ir::{Comp, PipeYield};
use crate::types::{Env, Mooring, Settled, Shell, Value};
use std::sync::Arc;
use std::time::Instant;

use collect::CollectState;
use group::PipelineGroup;
use launch::{PipelineStart, launch_pipeline};
use resolve::resolve_pipeline;

/// A multi-stage pipeline between its launch and its join, its stages running
/// in their own process group.
///
/// Field order is teardown order: a collector dropped mid-flight kills the
/// pgid, which must still be the anchor's — a reaped leader's pid may be
/// reused once its group is empty.
pub(crate) struct PipeNode {
    collect: CollectState,
    group: PipelineGroup,
    yields: PipeYield,
}

impl PipeNode {
    /// Resolve the plan and spawn every stage into one process group, none
    /// observed yet.  No stage runs in the parent, so none is in tail
    /// position.
    ///
    /// `env` is the pipeline node's own lexical environment — the machine's
    /// `E` in focus, not necessarily `shell.env` — and is what a stage
    /// thread's closure captures.
    pub(crate) fn launch(
        stages: &[Arc<Comp>],
        yields: PipeYield,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        // A top-level pipeline sits under no Bind, so this is its first
        // signal-checked seam.
        crate::process::check(mooring)?;
        let plan = resolve_pipeline(stages, env, mooring, shell)?;

        // Window start for the sandbox-denial reader: before any stage spawns,
        // so a deny the kernel logs for one falls inside it.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();

        // Whether this pipeline owns its pgid or joins an enclosing stage's.
        let group = match shell.io.launch_role.stage_group() {
            Some(g) => PipelineGroup::joining(g),
            None => PipelineGroup::prepare(shell, tx.clone())?,
        };

        let start = PipelineStart {
            group,
            yields,
            tx,
            rx,
            started,
        };
        launch_pipeline(stages, &plan, start, env, mooring, shell)
    }

    /// Wait on every stage and fold their observations into one value.
    pub(crate) fn join(mut self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        self.collect.drive(&self.group);
        self.collect.fold(mooring, shell).finish(self.yields)
    }
}
