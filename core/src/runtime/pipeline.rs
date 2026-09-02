//! Pipeline execution engine.  `resolve` freezes the whole plan — the form's
//! yield and each stage's launch decision; `launch` walks the stages once,
//! placing every one in a single process group; `join` (`collect` then
//! `finish`) waits in launch order, surfaces the first error, and recovers
//! the final value.  [`PipeNode`] is the orchestrator; nothing more.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
#[cfg(unix)]
pub(crate) mod parked;
pub(crate) mod resolve;
mod route;
mod thread;

use crate::ir::{Comp, PipeYield};
use crate::process::StageGate;
#[cfg(unix)]
use crate::types::{Break, Escape};
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
    gate: Arc<StageGate>,
    running: Running,
    yields: PipeYield,
    started: Instant,
    /// The pipeline's rendered source, for `ParkedPipeline`'s job-table name.
    cmd: String,
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

        // Two independent axes: whether this pipeline owns its pgid or joins
        // an enclosing stage's, and whether its Ctrl-Z gate is fresh or that
        // stage's own.  Every combination is meaningful — a `spawn` worker
        // inside a stage joins the group but carries no park.
        let group = match shell.io.launch_role.stage_group() {
            Some(g) => PipelineGroup::joining(g),
            None => PipelineGroup::prepare(plan.terminal, shell)?,
        };
        let gate = match &mooring.park {
            Some(p) => Arc::clone(&p.gate),
            None => StageGate::new(),
        };
        let cmd = render_cmd(shell, stages);

        let (group, running) = launch_pipeline(stages, &plan, env, mooring, shell, group, &gate)?;
        Ok(Self {
            group,
            gate,
            running,
            yields: plan.yields,
            started,
            cmd,
        })
    }

    /// Wait on every stage and fold the outcome into one value.
    ///
    /// The last stage carries its value home directly, on the `JoinHandle` or
    /// the OS wait alike; collect reads it only after waiting on the stage,
    /// since one blocked on a stopped upstream would deadlock us. A stop
    /// parks the pipeline instead of returning: the terminal goes back to the
    /// shell, the state is deposited under its pgid, and `Escape::Stopped`
    /// propagates exactly as a foreground external's stop would.
    pub(crate) fn join(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let Self {
            mut group,
            gate,
            running,
            yields,
            started,
            cmd,
        } = self;
        // `cmd` names a parked pipeline's job entry — Unix only, since there
        // is no park to name off Unix.
        #[cfg(not(unix))]
        let _ = &cmd;
        let mut collect = collect::CollectState::new(running, started);
        match collect.drive(&mut group, &gate, mooring, shell) {
            collect::Drive::Done => collect.fold(mooring, shell).finish(yields),
            #[cfg(unix)]
            collect::Drive::Parked(signal) => {
                group.release_foreground_and_relay();
                let pgid = group.leader_pgid();
                shell.park_pipeline(parked::ParkedPipeline {
                    group,
                    gate,
                    collect,
                    yields,
                    cmd: cmd.clone(),
                });
                Err(Break::Escape(Escape::Stopped {
                    pgid,
                    signal,
                    cmd,
                    pending: Vec::new(),
                }))
            }
        }
    }
}

/// The pipeline's own source text, spanning its first stage's start to its
/// last stage's end, for a parked job's display name; `"ral pipeline"`
/// when a stage carries no span or its file is unregistered.
fn render_cmd(shell: &Shell, stages: &[Arc<Comp>]) -> String {
    let bounds = stages
        .first()
        .and_then(|s| s.span)
        .zip(stages.last().and_then(|s| s.span));
    bounds
        .and_then(|(first, last)| {
            let source = shell.session.sources.get(first.file)?;
            let text = source.as_str();
            let end = (last.end as usize).min(text.len());
            text.get(first.start as usize..end)
        })
        .map_or_else(|| "ral pipeline".to_string(), ToString::to_string)
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
