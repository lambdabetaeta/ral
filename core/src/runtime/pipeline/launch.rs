//! Process-staged pipeline orchestrator: every stage spawns into one
//! process group behind a gate that opens only once the last has spawned
//! and the terminal handoff has settled.  Pure-value pipelines fold in
//! the parent evaluator and never arrive here.  [`PipelineBuild`] owns
//! every transient resource — group, stage handles, unconsumed routes,
//! unreleased gates — so a leaked pipe end is a borrow error.

use super::super::command;
use super::collect::RunningPipeline;
use super::group::PipelineGroup;
use super::protocol::DeferredFrame;
use super::resolve::{ExternalStage, PipelinePlan, StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, FinalValue, StageRoute, open_stage_routes};
use super::stage::{HelperStageHandle, launch_helper_stage};
use crate::child_eval::pack_request;
use crate::io::Sink;
use crate::types::{Break, Mooring, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;

pub(super) enum StageHandle {
    External(command::RunningChild),
    Helper(HelperStageHandle),
}

/// Route stdin for the stage on the pipeline's input boundary, consuming
/// whatever `<file` or parent pipe sits on `shell.io.stdin` — otherwise
/// `f < file` on a function whose body is a pipeline drops the redirect.
/// Unlike `command::stdio::wire_stdin`, a tty fd 0 is inherited only when
/// this pgid will own the terminal; a backgrounded reader takes SIGTTIN.
fn route_parent_stdin(group: &PipelineGroup, shell: &mut Shell) -> command::StdinRoute {
    // `Source::Empty` — an exarch tool run — denies byte input outright.
    if matches!(shell.io.stdin, crate::io::Source::Empty) {
        return command::StdinRoute::Null;
    }
    match shell.io.stdin.take_reader() {
        Some(crate::io::SourceReader::Pipe(r)) => command::StdinRoute::Pipe(r),
        Some(crate::io::SourceReader::File(f)) => command::StdinRoute::File(f),
        None if !shell.io.terminal.startup_stdin_tty => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_non_tty_stdin())
        }
        None if group.owns_tty() => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_pure_external_pipeline())
        }
        None => command::StdinRoute::Null,
    }
}

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

/// Wire a stage's stdout against its [`ByteOut`] edge, returning the sink
/// the parent must pump when the boundary stdout is not a plain fd.
pub(super) fn wire_stage_stdout(
    cmd: &mut crate::process::Launch,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &Shell,
) -> Settled<Option<Sink>> {
    match stdout {
        ByteOut::Downstream(writer) => {
            cmd.stdout(crate::process::StdioSpec::from_pipe_writer(writer));
            Ok(None)
        }
        ByteOut::Parent => {
            // Inherit ral's real fd 1 so a pager or `ls` still sees a TTY —
            // the pipeline analogue of `command::stdio::inherit_tty`.
            let inherit = shell.io.terminal.startup_stdout_tty && group.owns_tty();
            let plan = shell
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

/// Wire one stage's stdin, stdout, and stderr from its route.  Shared with
/// `stage::launch_helper_stage` so helper and direct wiring cannot drift.
pub(super) fn wire_stage_stdio(
    cmd: &mut crate::process::Launch,
    stdin: ByteIn,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &mut Shell,
) -> Settled<command::ExternalPlumbing> {
    cmd.stdin(route_stdin(stdin, group, shell).into_stdio());
    let stdout_pump = wire_stage_stdout(cmd, stdout, group, shell)?;
    let stderr_plan = shell
        .io
        .stderr
        .child_stderr()
        .map_err(super::protocol::pipe_error)?;
    cmd.stderr(stderr_plan.stdio);
    Ok(command::ExternalPlumbing {
        stdout_pump,
        stderr_pump: stderr_plan.pump,
    })
}

/// Spawn `cmd` into `group` and assemble the [`command::RunningChild`] —
/// the one funnel for both direct externals and ral helpers.
#[allow(
    clippy::too_many_arguments,
    reason = "the single funnel for post-spawn assembly; splitting it would scatter the same parameters"
)]
pub(super) fn spawn_into_group(
    group: &mut PipelineGroup,
    cmd: &mut crate::process::Launch,
    name: String,
    plumbing: command::ExternalPlumbing,
    mooring: &Mooring,
    shell: &Shell,
    park_on_stop: bool,
    spawn_error: impl FnOnce(std::io::Error) -> Break,
) -> Settled<command::RunningChild> {
    let (child, jail) = group.spawn(cmd).map_err(spawn_error)?;
    if shell.has_active_capabilities() {
        // Windows routes the limits through the pipeline's own job rather
        // than a second per-child one; on Unix `pre_exec` did it already.
        crate::sandbox::apply_child_limits_in_pipeline(&child, group.leader_pgid());
    }
    Ok(command::RunningChild::assemble_with_owner(
        child,
        group.leader_pgid(),
        name,
        plumbing,
        park_on_stop,
        // Windows group release belongs to `PipelineGroup::drop`.
        command::GroupOwner::BorrowedByPipeline,
        mooring.cancel.as_scope().clone(),
        jail,
    ))
}

struct LaunchCx<'a> {
    mooring: &'a Mooring,
    shell: &'a mut Shell,
    group: &'a mut PipelineGroup,
    park_on_stop: bool,
}

/// One stage's launch product: the handle to collect, and the gate frame
/// held back until every stage has spawned.
struct SpawnedStage {
    handle: StageHandle,
    gate: Option<DeferredFrame>,
}

/// Dispatch one stage per its resolve-time [`StageLaunch`] — a direct
/// external spawn, or a helper carrying a packed eval request.
#[allow(
    clippy::needless_pass_by_value,
    reason = "LaunchCx bundles unique `&mut` borrows; by-value transfers them so callees get mutable access — a shared `&LaunchCx` cannot yield `&mut`"
)]
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: LaunchCx<'_>,
) -> Settled<SpawnedStage> {
    let request = match &spec.launch {
        StageLaunch::Direct(ext) => {
            let handle = launch_external_stage_direct(
                ext,
                route,
                cx.mooring,
                cx.shell,
                cx.group,
                cx.park_on_stop,
            )?;
            return Ok(SpawnedStage {
                handle: StageHandle::External(handle),
                gate: None,
            });
        }
        StageLaunch::HelperEval => {
            let captured = cx.shell.snapshot();
            // Only the final value-typed stage ships a return value home.
            let wants_value = matches!(route.final_value, FinalValue::Report);
            pack_request(
                Arc::clone(stage),
                &cx.shell.mobile,
                Some(&captured),
                cx.shell.local.audit.active_policy(),
                wants_value,
                stage.span,
                &cx.shell.session.sources,
            )?
        }
    };
    let (handle, deferred) = launch_helper_stage(
        request,
        spec,
        route,
        cx.mooring,
        cx.shell,
        cx.group,
        cx.park_on_stop,
    )?;
    Ok(SpawnedStage {
        handle: StageHandle::Helper(handle),
        gate: Some(deferred),
    })
}

/// Partial-launch resources in teardown order: Rust drops fields top to
/// bottom, and that order is the invariant.  Gates close first so parked
/// helpers see EOF and stand down, then unconsumed routes so half-wired
/// neighbours see EOF, then the children; the pgid anchor outlives all.
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
    #[allow(
        clippy::unused_self,
        reason = "the signature is the Unix arm's, which reads the group's leader pgid off `self`; off Unix there is no process group to signal"
    )]
    fn signal_group(&self) {}
}

/// Linear accumulator: one [`PipelineBuild::step`] per stage, then
/// `finish`.  Holding the sole handle to the group, the routes, and the
/// gates makes a leak a borrow error.  `finish` is the only site of both
/// `tcsetpgrp` and gate release, so no stage runs user code before it.
struct PipelineBuild {
    resources: PipelineResources,
    park_on_stop: bool,
}

impl PipelineBuild {
    fn new(plan: &PipelinePlan, routes: VecDeque<StageRoute>, shell: &Shell) -> Settled<Self> {
        let mut group = PipelineGroup::new(plan.terminal);
        // The anchor holds the pgid open across the launch so a later stage's
        // `setpgid` target cannot die first; no-op off Unix and for one stage.
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
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        let route = self
            .resources
            .routes
            .pop_front()
            .expect("one route per stage");
        let cx = LaunchCx {
            mooring,
            shell,
            group: &mut self.resources.group,
            park_on_stop: self.park_on_stop,
        };
        let spawned = spawn_stage(stage, spec, route, cx)?;
        self.resources.deferred_jobs.extend(spawned.gate);
        self.resources.running.add(spawned.handle);
        Ok(())
    }

    /// Tear down a partially-launched pipeline.  SIGTERM the pgid first so
    /// helpers and externals that honour it leave before the drop order
    /// reaches SIGKILL; [`PipelineResources`]'s field order does the rest.
    fn abort(self) {
        let Self { resources, .. } = self;
        resources.signal_group();
        drop(resources);
    }

    /// Hand the terminal to the pipeline pgid, release every gate, and return
    /// the group alongside — its anchor and guards must outlive collect.
    fn finish(
        self,
        shell: &Shell,
        mooring: &Mooring,
    ) -> Result<(PipelineGroup, RunningPipeline), Break> {
        let Self { mut resources, .. } = self;
        // Foreground before releasing the gates, so the kernel's foreground
        // decision is settled when stages start running user code.
        resources.group.claim_foreground(shell, mooring);
        for deferred in std::mem::take(&mut resources.deferred_jobs) {
            deferred.release()?;
        }
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

/// Spawn a pure external stage with no helper in front of it.  Admitted by
/// `resolve::direct_spawnable` alone: no value edge, no redirect, no
/// byte-capturing audit, no terminal to hand over.
fn launch_external_stage_direct(
    ext: &ExternalStage,
    route: StageRoute,
    mooring: &Mooring,
    shell: &mut Shell,
    group: &mut PipelineGroup,
    park_on_stop: bool,
) -> Result<command::RunningChild, Break> {
    // A value-carrying stage never gets here, so the route is byte ends only.
    debug_assert!(route.value_in.is_none() && route.value_out.is_none());

    let rc = command::vet(&ext.id, &ext.args, shell)?;
    let mut cmd = command::build_command(&rc, crate::sandbox::Ownership::Kept, shell)?;

    // Nor a redirect, so `ext` carries none and there is no file to open.
    let plumbing = wire_stage_stdio(&mut cmd, route.stdin, route.stdout, group, shell)?;

    spawn_into_group(
        group,
        &mut cmd,
        rc.shown.clone(),
        plumbing,
        mooring,
        shell,
        park_on_stop,
        |e| command::spawn_error(&rc.shown, &e),
    )
}

/// Spawn every stage, polling for cancellation before each.  Until the first
/// `PipelineGroup::spawn` claims the SIGINT relay slot a SIGINT only cancels
/// the run's foreground scope; this poll turns that into a prompt abort.
fn spawn_all_stages(
    build: &mut PipelineBuild,
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    for (ix, stage) in stages.iter().enumerate() {
        crate::process::check(mooring)?;
        build.step(stage, &plan.specs[ix], mooring, shell)?;
    }
    Ok(())
}

/// Launch every stage into the pipeline's process group; a mid-launch error
/// goes to [`PipelineBuild::abort`] for the ordered teardown.
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<(PipelineGroup, RunningPipeline), Break> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(plan, routes, shell)?;
    match spawn_all_stages(&mut build, stages, plan, mooring, shell) {
        Ok(()) => build.finish(shell, mooring),
        Err(e) => {
            build.abort();
            Err(e)
        }
    }
}
