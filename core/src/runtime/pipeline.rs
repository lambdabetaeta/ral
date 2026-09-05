//! Pipeline execution engine.  `resolve` freezes the terminal handoff and each
//! stage's launch decision, `launch` places every stage in one process group,
//! `join` folds their observations into one value.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
pub(crate) mod resolve;
mod route;
mod sentinel;
mod stage;
mod thread;

use crate::ir::{Comp, PipeYield};
use crate::types::{Env, Mooring, Settled, Shell, Value};
use std::sync::Arc;
use std::time::Instant;

use collect::{CollectState, Slot};
use group::PipelineGroup;
use launch::{LaunchCx, spawn_stage};
use resolve::{TerminalPlan, resolve_pipeline};
use route::open_stage_routes;

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
        crate::process::check(mooring)?;
        let plan = resolve_pipeline(stages, env, mooring, shell)?;

        // Window start for the sandbox-denial reader.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();

        let mut group = match shell.io.launch_role.stage_group() {
            Some(g) => PipelineGroup::joining(g),
            None => PipelineGroup::prepare(shell, tx.clone())?,
        };
        if plan.terminal == TerminalPlan::ForegroundExternalGroup {
            group.claim_foreground(shell, mooring);
        }

        let collect = CollectState::new(rx, &tx, group.owned_pgid(), mooring, started);
        let mut node = Self {
            collect,
            group,
            yields,
        };
        // Declared after `node` so unconsumed routes close before it tears
        // down: a half-wired neighbour must see EOF.
        let routes = open_stage_routes(stages.len())?;

        let mut cx = LaunchCx {
            mooring,
            shell,
            env,
            group: &node.group,
        };
        for (ix, ((stage, spec), route)) in stages.iter().zip(&plan.specs).zip(routes).enumerate() {
            crate::process::check(mooring)?;
            let handle = spawn_stage(stage, spec, route, &mut cx, Slot { ix, tx: tx.clone() })?;
            node.collect.push(handle);
        }
        Ok(node)
    }

    /// Wait on every stage and fold their observations into one value.
    pub(crate) fn join(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let Self {
            mut collect,
            group,
            yields,
        } = self;
        collect.drive();
        let value = collect.fold(mooring, shell, yields);
        drop(group);
        value
    }
}
