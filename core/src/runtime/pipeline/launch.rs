//! Process-staged pipeline orchestrator.
//!
//! Pure-value pipelines never enter here.  Everything else is reduced
//! through a [`PipelineBuild`] accumulator: one `step` per stage, one
//! `finish` to release the launch gate.  The accumulator owns every
//! transient resource — the pipeline group, the running-pipeline
//! collector, the unconsumed stage routes, and the deferred-frame
//! backlog — so leak-prone wiring is structurally impossible.
//!
//! A direct external stage is allowed only when it needs no helper-owned
//! work: no value edge, no redirect, no byte audit capture, and no
//! foreground terminal handoff.  Every other stage runs in a ral helper.

use super::super::command;
use super::collect::RunningPipeline;
use super::group::PipelineGroup;
use super::protocol::DeferredFrame;
use super::resolve::{ExternalStage, PipelinePlan, StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, FinalValue, StageRoute, open_stage_routes};
use super::stage::{HelperStageHandle, launch_helper_stage};
use crate::child_eval::pack_request;
use crate::io::Sink;
use crate::types::{Break, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;

pub(super) enum StageHandle {
    External(command::RunningChild),
    Helper(HelperStageHandle),
}

/// Route stdin for a stage whose input side is the pipeline boundary.
///
/// This mirrors `command::wire_stdin` for standalone externals: if the
/// enclosing call has parked a `<file` redirect or a parent-shell pipe
/// on `shell.turn.io.stdin`, the boundary stage consumes it.  Without this,
/// `f < file` on a function whose body is a pipeline would silently
/// drop the redirect — the pipeline saw only `Parent` without ever
/// reading the source.  Pipeline policy diverges from standalone exec
/// only in the inherit branch: a foreground pipeline's pgid is what
/// owns the tty, so the permit kind reflects that.
fn route_parent_stdin(group: &PipelineGroup, shell: &mut Shell) -> command::StdinRoute {
    // An explicit empty source (an exarch tool turn) wires the boundary stage
    // to `/dev/null` — no fd-0 fall-through, mirroring `command::wire_stdin`.
    if matches!(shell.turn.io.stdin, crate::io::Source::Empty) {
        return command::StdinRoute::Null;
    }
    match shell.turn.io.stdin.take_reader() {
        Some(crate::io::SourceReader::Pipe(r)) => command::StdinRoute::Pipe(r),
        Some(crate::io::SourceReader::File(f)) => command::StdinRoute::File(f),
        None if !shell.turn.io.terminal.startup_stdin_tty => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_non_tty_stdin())
        }
        None if group.owns_tty() => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_pure_external_pipeline())
        }
        None => command::StdinRoute::Null,
    }
}

/// Resolve a stage's stdin source: an `Upstream` edge moves its reader
/// straight in; a `Parent` boundary routes the enclosing stdin
/// (file / pipe / inherit) via [`route_parent_stdin`].
pub(super) fn route_stdin(
    stdin: ByteIn,
    group: &PipelineGroup,
    shell: &mut Shell,
) -> command::StdinRoute {
    match stdin {
        ByteIn::Upstream(r) => command::StdinRoute::Pipe(r),
        ByteIn::Parent => route_parent_stdin(group, shell),
    }
}

/// Wire a stage's stdout against its [`ByteOut`] edge, returning the pump
/// sink the parent must drain when the boundary stdout is non-fd.
/// Shared by the direct-spawn external path and the ral-helper path: a
/// `Downstream` edge moves its writer straight into the child; a `Parent`
/// boundary routes through the shell's stdout child plan; a `Null` stage
/// (value-out, no bytes) discards to `/dev/null`.
pub(super) fn wire_stage_stdout(
    cmd: &mut crate::process::Launch,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &mut Shell,
) -> Settled<Option<Sink>> {
    match stdout {
        ByteOut::Downstream(writer) => {
            cmd.stdout(crate::process::StdioSpec::from_pipe_writer(writer));
            Ok(None)
        }
        ByteOut::Parent => {
            // The final byte stage inherits ral's fd 1 directly — so a pager
            // or `ls`/`grep` sees a TTY — under the same predicate the
            // standalone path uses (`stdio::inherit_tty`): fd 1 was a tty at
            // startup and this group owns the terminal foreground.
            let inherit = shell.turn.io.terminal.startup_stdout_tty && group.owns_tty();
            let plan = shell
                .turn
                .io
                .stdout
                .child_stdout(inherit)
                .map_err(super::protocol::pipe_error)?;
            cmd.stdout(plan.stdio);
            Ok(plan.pump)
        }
        ByteOut::Null => {
            cmd.stdout(crate::process::StdioSpec::null());
            Ok(None)
        }
    }
}

/// Spawn `cmd` into `group`, apply post-spawn child limits if any
/// capability layer is active, and assemble a [`command::RunningChild`]
/// with the parent-side pumps attached.  One funnel for both external
/// and ral stages so the post-spawn boilerplate cannot drift.
pub(super) fn spawn_into_group(
    group: &mut PipelineGroup,
    cmd: &mut crate::process::Launch,
    name: String,
    plumbing: command::ExternalPlumbing,
    shell: &Shell,
    park_on_stop: bool,
    spawn_error: impl FnOnce(std::io::Error) -> Break,
) -> Settled<command::RunningChild> {
    let child = group.spawn(cmd).map_err(spawn_error)?;
    if shell.has_active_capabilities() {
        // Pipeline path: any active grant routes its limits through the
        // pipeline's job (Windows) instead of assigning the child to a
        // second job; on Unix the limits are pre_exec already and this
        // call is a no-op.
        crate::sandbox::apply_child_limits_in_pipeline(&child, group.leader_pgid());
    }
    Ok(command::RunningChild::assemble_with_owner(
        child,
        group.leader_pgid(),
        name,
        plumbing,
        park_on_stop,
        // Pipeline stages borrow the group; `PipelineGroup::Drop`
        // owns the release.
        command::GroupOwner::BorrowedByPipeline,
        shell.turn.cancel.as_scope().clone(),
    ))
}

/// Per-stage launch context borrowed by [`spawn_stage`].
struct LaunchCx<'a> {
    shell: &'a mut Shell,
    group: &'a mut PipelineGroup,
    park_on_stop: bool,
}

/// Resources produced by launching one stage.
///
/// The build accumulator consumes this as a linear transition: the
/// stage handle is collected and `gate` is queued until every stage
/// has spawned.
struct SpawnedStage {
    handle: StageHandle,
    gate: Option<DeferredFrame>,
}

/// Dispatch a stage to its launcher according to the resolve-time
/// [`StageLaunch`] decision: a pure external command spawned directly,
/// or a helper stage (a bundled tool or a ral computation).  The route
/// is consumed whole — its edge ends move into the spawned child's
/// stdio and protocol channels.
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: LaunchCx<'_>,
) -> Settled<SpawnedStage> {
    let request = match &spec.launch {
        StageLaunch::Direct(ext) => {
            let handle =
                launch_external_stage_direct(ext, route, cx.shell, cx.group, cx.park_on_stop)?;
            return Ok(SpawnedStage {
                handle: StageHandle::External(handle),
                gate: None,
            });
        }
        StageLaunch::HelperEval => {
            let captured = cx.shell.snapshot();
            // Pipeline stages are subshells: only the final value-typed
            // stage ships its return value (`wants_value`).
            let wants_value = matches!(route.final_value, FinalValue::Report);
            pack_request(
                Arc::clone(stage),
                &cx.shell.mobile,
                Some(&captured),
                cx.shell.local.audit.active_policy(),
                wants_value,
                &cx.shell.turn.loc,
            )?
        }
    };
    let (handle, deferred) =
        launch_helper_stage(request, spec, route, cx.shell, cx.group, cx.park_on_stop)?;
    Ok(SpawnedStage {
        handle: StageHandle::Helper(handle),
        gate: Some(deferred),
    })
}

/// Partial-launch resources in their safe teardown order.
///
/// Rust drops fields top-to-bottom.  That order is the invariant:
/// unreleased stage gates close first, then the unconsumed stage
/// routes (closing every unspawned stage's edge ends, so any
/// half-wired neighbour sees EOF), then stage handles are
/// killed/reaped, and only then may the group/anchor be waited.
/// Keeping these fields under one owner makes the abort order the
/// ordinary ownership shape, not a convention at each error site.
struct PipelineResources {
    deferred_jobs: Vec<DeferredFrame>,
    routes: VecDeque<StageRoute>,
    running: RunningPipeline,
    group: PipelineGroup,
}

impl PipelineResources {
    fn new(group: PipelineGroup, routes: VecDeque<StageRoute>) -> Self {
        Self {
            deferred_jobs: Vec::new(),
            routes,
            running: RunningPipeline::new(),
            group,
        }
    }

    #[cfg(unix)]
    fn signal_group(&self) {
        if let Some(pgid) = self.group.leader_pgid() {
            pgid.signal_group(crate::process::Signal::new(libc::SIGTERM));
        }
    }

    #[cfg(not(unix))]
    fn signal_group(&self) {}
}

/// Linear accumulator that drives launch through one stage at a time.
///
/// Each [`PipelineBuild::step`] consumes the next pre-built
/// [`StageRoute`] and advances exactly one stage.  Owning every
/// transient resource (group, running, deferred jobs, unconsumed
/// routes) here makes pipe leaks and stranded helpers structurally
/// impossible — the borrow checker enforces the linear-resource
/// invariant.
///
/// `finish` is the only place `tcsetpgrp` runs and the only place the
/// deferred frames are written: gate-release happens once, after every
/// stage has spawned.
struct PipelineBuild {
    resources: PipelineResources,
    park_on_stop: bool,
}

impl PipelineBuild {
    fn new(plan: &PipelinePlan, routes: VecDeque<StageRoute>, shell: &Shell) -> Settled<Self> {
        let mut group = PipelineGroup::new(plan.terminal);
        // Spawn the pgid anchor (no-op off Unix and for a single
        // stage).  The anchor holds the pgid for the whole launch
        // sequence, so a stage's `setpgid` join target cannot die
        // before launch completes.
        group.prepare(shell, plan.specs.len())?;
        let park_on_stop = plan.terminal.owns_tty();
        Ok(Self {
            resources: PipelineResources::new(group, routes),
            park_on_stop,
        })
    }

    fn step(
        &mut self,
        stage: &Arc<crate::ir::Comp>,
        spec: &StageSpec,
        shell: &mut Shell,
    ) -> Settled<()> {
        let route = self
            .resources
            .routes
            .pop_front()
            .expect("one route per stage");
        let cx = LaunchCx {
            shell,
            group: &mut self.resources.group,
            park_on_stop: self.park_on_stop,
        };
        let spawned = spawn_stage(stage, spec, route, cx)?;
        self.resources.deferred_jobs.extend(spawned.gate);
        self.resources.running.add(spawned.handle);
        Ok(())
    }

    /// Tear down a partially-launched pipeline after a mid-launch error.
    ///
    /// Order is load-bearing.  SIGTERM the pgid first so already-spawned
    /// helpers and direct externals that respect it exit promptly.
    /// Then close the unreleased stage gates: a helper parked on its
    /// job read treats EOF as the parent's "stand down" message and
    /// exits, closing the inherited anchor fd on the way out.  The
    /// unconsumed stage routes drop next so any half-wired neighbour
    /// sees EOF.  Only then do we drop the running handles, whose abort
    /// path kills, joins pump threads, and reaps.  The group drops
    /// last, after the anchor can observe channel EOF (or the abort
    /// kill) and be waited without forming a cycle.
    fn abort(self) {
        let Self { resources, .. } = self;
        resources.signal_group();
        drop(resources);
    }

    /// Foreground the pipeline pgid (when interactive) and release every
    /// gate frame.  Returns the running pipeline, with the
    /// `PipelineGroup` alongside so its anchor, foreground guard, and
    /// relay stay alive through collect.
    fn finish(self, shell: &mut Shell) -> Result<(PipelineGroup, RunningPipeline), Break> {
        let Self { mut resources, .. } = self;
        // Hand the controlling tty to the pipeline pgid (interactive
        // only) *before* releasing the gate frames so the kernel's
        // foreground decision is settled when stages start running
        // user code.
        resources.group.claim_foreground(shell);
        let mut deferred_jobs = std::mem::take(&mut resources.deferred_jobs);
        for deferred in deferred_jobs.drain(..) {
            deferred.release()?;
        }
        drop(deferred_jobs);
        let PipelineResources {
            deferred_jobs,
            routes,
            running,
            group,
        } = resources;
        debug_assert!(deferred_jobs.is_empty());
        debug_assert!(routes.is_empty());
        Ok((group, running))
    }
}

/// Spawn a direct external stage: no stage helper, no
/// serialisation.  Only used for `NoTerminal` pipelines where there
/// is no foreground gating and no ral code to evaluate.
fn launch_external_stage_direct(
    ext: &ExternalStage,
    route: StageRoute,
    shell: &mut Shell,
    group: &mut PipelineGroup,
    park_on_stop: bool,
) -> Result<command::RunningChild, Break> {
    // `resolve::direct_spawnable` routes any value-carrying stage
    // through the helper, so a direct-spawn route holds byte ends only.
    debug_assert!(route.value_in.is_none() && route.value_out.is_none());

    let rc = command::vet(&ext.id, &ext.args, shell)?;
    let mut cmd = command::build_command(&rc, shell)?;

    cmd.stdin(route_stdin(route.stdin, group, shell).into_stdio());

    // Stdout: pipe to downstream > pump to parent > null.
    // Redirect-file branches are unreachable here — the caller
    // gates on `ext.redirects.is_empty()`.
    let stdout_pump = wire_stage_stdout(&mut cmd, route.stdout, group, shell)?;

    let stderr_piped = !matches!(shell.turn.io.stderr, Sink::Stderr);
    cmd.stderr(if stderr_piped {
        crate::process::StdioSpec::piped()
    } else {
        crate::process::StdioSpec::inherit()
    });

    let stderr_pump = if stderr_piped {
        Some(
            shell
                .turn
                .io
                .stderr
                .try_clone()
                .map_err(super::protocol::pipe_error)?,
        )
    } else {
        None
    };

    spawn_into_group(
        group,
        &mut cmd,
        rc.shown.clone(),
        command::ExternalPlumbing {
            stdout_pump,
            stderr_pump,
        },
        shell,
        park_on_stop,
        |e| command::spawn_error(&rc.shown, e),
    )
}

/// Spawn every stage into the build's pgid.  Per-stage SIGINT/cancel
/// check at the top of each iteration — the relay slot is claimed
/// inside `PipelineGroup::spawn` only once a real child has joined the
/// pgid (see `group.rs`'s SIGINT/relay invariant), so SIGINTs arriving
/// before that claim — between `prepare` and the first `spawn`, or
/// between a stage spawn returning and the next stage starting — only
/// cancel the turn's foreground scope.  Polling that scope at the top
/// of every iteration aborts launch promptly without leaving
/// anchor-only or partially-spawned groups stranded.
fn spawn_all_stages(
    build: &mut PipelineBuild,
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    shell: &mut Shell,
) -> Settled<()> {
    for (ix, stage) in stages.iter().enumerate() {
        crate::process::check(shell)?;
        build.step(stage, &plan.specs[ix], shell)?;
    }
    Ok(())
}

/// Launch every byte-capable stage into the pipeline's process group.
///
/// On any error mid-launch, [`PipelineBuild::abort`] explicitly orders
/// teardown: signal the pgid, close unreleased gates and pipe ends,
/// reap stage children, then reap the anchor.  Keeping the anchor last
/// avoids a deadlock when a helper inherited the anchor channel but is
/// still blocked waiting for the never-released stage job.
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    shell: &mut Shell,
) -> Result<(PipelineGroup, RunningPipeline), Break> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(plan, routes, shell)?;
    match spawn_all_stages(&mut build, stages, plan, shell) {
        Ok(()) => build.finish(shell),
        Err(e) => {
            build.abort();
            Err(e)
        }
    }
}
