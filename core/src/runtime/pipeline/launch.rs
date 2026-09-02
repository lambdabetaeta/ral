//! Process-staged pipeline orchestrator: every stage — a ral-written thread
//! or a direct external — spawns into one process group, joined or owned.
//! [`PipelineBuild`] owns every transient resource — group, stage handles,
//! unconsumed routes — so a leaked pipe end is a borrow error.

use super::super::command;
use super::collect::{StageObservation, observe_external_stage};
use super::group::PipelineGroup;
use super::resolve::{ExternalStage, PipelinePlan, StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, StageRoute, open_stage_routes};
use super::thread::{ThreadStage, launch_thread_stage};
use crate::io::{Sink, Source, SourceReader};
use crate::process::{CancelCause, Ending, EndingCell, Signal, StageGate, StopPolicy};
use crate::types::{Break, Env, Error, Mooring, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;

/// Whether a stage is ready to observe, still running, or stopped — the
/// collector's per-pass classification of one [`StageHandle`].
pub(super) enum Probe {
    Running,
    Ready,
    Stopped(Signal),
}

/// One stage's process — external or thread — paired with the parent's
/// duplicate of its outbound edge's read end, in the collector's care until
/// this stage's observation completes.
pub(super) struct StageHandle {
    kind: StageKind,
    held_edge: Option<os_pipe::PipeReader>,
    /// Mirrors `StageSpec::feeds_pipe`: whether this stage's stdout can
    /// still reach the interior edge, so the collector knows when a dead
    /// reader downstream is none of this stage's business at all.
    feeds_pipe: bool,
    /// Private to this module: only [`StageHandle::reader_gone`] can record
    /// the one ending that is forgiven, so no stage is forgiven a death
    /// nothing sent it.
    ending: EndingCell,
}

enum StageKind {
    External(command::RunningChild),
    Thread(ThreadStage),
}

impl StageHandle {
    /// This stage's reader has been observed, so nothing it still produces is
    /// owed to anybody: record the ending and end it.  Only reached for a
    /// stage that just probed `Running` and whose stdout can still reach the
    /// interior edge, so a stage that already finished keeps its outcome and
    /// no exit status is ever forgiven.
    pub(super) fn reader_gone(&mut self) {
        if !self.feeds_pipe || self.ending.get() != Ending::OwnAccord {
            return;
        }
        self.ending.raise(Ending::RalEnded(CancelCause::ReaderGone));
        match &mut self.kind {
            StageKind::External(c) => c.reader_gone(),
            StageKind::Thread(t) => {
                t.cancel(CancelCause::ReaderGone);
                t.interrupt();
            }
        }
    }

    /// End this stage as part of the pipeline's teardown; the caller signals
    /// and kills the group separately.  A thread is also woken, which ends
    /// the read or write it is blocked in.
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        self.ending.raise(Ending::RalEnded(cause));
        match &self.kind {
            StageKind::External(c) => c.cancel.cancel(cause),
            StageKind::Thread(t) => {
                t.cancel(cause);
                t.interrupt();
            }
        }
    }

    /// One non-blocking probe: whether this stage is ready to observe, still
    /// running, or stopped.  An external's stop is read back from the outcome
    /// `try_settle` remembered; a thread's is read live.
    pub(super) fn probe(&mut self) -> Probe {
        match &mut self.kind {
            StageKind::External(c) => {
                if !c.try_settle() {
                    return Probe::Running;
                }
                c.remembered_stop().map_or(Probe::Ready, Probe::Stopped)
            }
            StageKind::Thread(t) => t.probe(),
        }
    }

    /// The collector answered this stage's stop with death.  A thread stage's
    /// stop is its own external's, which its `wait` is holding at the gate and
    /// the cancel will release; only a direct external must be settled here.
    pub(super) fn end_stopped(&mut self, sig: Signal) {
        #[cfg(unix)]
        if let StageKind::External(c) = &mut self.kind {
            c.end_stopped(sig);
        }
        #[cfg(not(unix))]
        unreachable!("nothing stops on Windows: {sig:?}");
    }

    /// `Stopped` → `Running`: a thread's status, or an external's remembered
    /// stop, forgotten so the next probe waits on it fresh.
    pub(super) fn resume(&mut self) {
        match &mut self.kind {
            StageKind::External(c) => c.clear_remembered_stop(),
            StageKind::Thread(t) => t.resume(),
        }
    }

    /// Reduce a settled stage to its observation, then release the held-open
    /// read end — only now that the writer is reaped, so any descendant of that
    /// edge still blocked writing into it is freed.
    pub(super) fn observe(
        self,
        shell: &Shell,
        is_last: bool,
        started: std::time::Instant,
    ) -> StageObservation {
        let Self {
            held_edge,
            kind,
            ending,
            ..
        } = self;
        let obs = match (kind, ending.get()) {
            (StageKind::External(c), _) => observe_external_stage(c, shell, started),
            // A thread's `Break` carries no mark of whether the kill or its
            // own code ended it, so a killed thread is forgiven whatever it
            // returned.
            (StageKind::Thread(t), Ending::RalEnded(CancelCause::ReaderGone)) => {
                t.observe(is_last).forgiven()
            }
            (StageKind::Thread(t), _) => t.observe(is_last),
        };
        drop(held_edge);
        obs
    }

    /// A `StageHandle` around an already-running external, for `collect.rs`'s
    /// own tests: they drive a real child's stop/settle through `probe`
    /// without a whole pipeline launch to set one up.
    #[cfg(test)]
    pub(super) fn for_test(child: command::RunningChild) -> Self {
        Self {
            kind: StageKind::External(child),
            held_edge: None,
            feeds_pipe: true,
            ending: EndingCell::default(),
        }
    }
}

/// Route stdin for the stage on the pipeline's input boundary, consuming
/// whatever `<file` or parent pipe sits on `shell.io.stdin` — otherwise
/// `f < file` on a function whose body is a pipeline drops the redirect.
/// Unlike `command::stdio::wire_stdin`, a tty fd 0 is inherited only when
/// this pgid will own the terminal; a backgrounded reader takes SIGTTIN.
fn route_parent_stdin(group: &PipelineGroup, shell: &Shell) -> Settled<command::StdinRoute> {
    // `Source::Empty` — an exarch tool run — denies byte input outright.
    if matches!(shell.io.stdin, Source::Empty) {
        return Ok(command::StdinRoute::Null);
    }
    let reader = shell
        .io
        .stdin
        .reader()
        .map_err(|e| Break::Error(Error::new(format!("could not duplicate stdin: {e}"), 1)))?;
    Ok(match reader {
        Some(r) => command::StdinRoute::Reader(r),
        None if !shell.io.terminal.startup_stdin_tty => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_non_tty_stdin())
        }
        None if group.holds_terminal() => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_pure_external_pipeline())
        }
        None => command::StdinRoute::Null,
    })
}

pub(super) fn route_stdin(
    stdin: ByteIn,
    group: &PipelineGroup,
    shell: &Shell,
) -> Settled<command::StdinRoute> {
    match stdin {
        ByteIn::Upstream(r) => Ok(command::StdinRoute::Reader(SourceReader::pipe(r))),
        ByteIn::Parent => route_parent_stdin(group, shell),
    }
}

/// A stage thread's stdin: the external's route, read in-process.  What a
/// child would inherit is duplicated, except a tty — a thread cannot read
/// the controlling terminal (SIGTTIN would stop the whole shell), so it sees
/// the EOF a capture already gives it.  The thread's own wake ends a blocked
/// read as EOF.
pub(super) fn stage_stdin(
    route: ByteIn,
    group: &PipelineGroup,
    shell: &Shell,
    wake: &Arc<crate::process::Wake>,
) -> Settled<Source> {
    let reader = match route_stdin(route, group, shell)? {
        command::StdinRoute::Reader(r) => r,
        command::StdinRoute::Null => return Ok(Source::Empty),
        command::StdinRoute::Inherit(_) if shell.io.terminal.startup_stdin_tty => {
            return Ok(Source::Empty);
        }
        command::StdinRoute::Inherit(_) => SourceReader::file(dup_stdin_file().map_err(|e| {
            Break::Error(Error::new(format!("could not duplicate stdin: {e}"), 1))
        })?),
    };
    Ok(Source::Reader(reader.interruptible(Arc::clone(wake))))
}

#[cfg(unix)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: fd 0 is always open in a running process; `dup` returns a
    // fresh, independently closable fd or -1 with errno set.
    let fd = unsafe { libc::dup(0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, HANDLE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: `GetStdHandle`/`GetCurrentProcess` return pseudo/real handles
    // valid for the whole process life; `DuplicateHandle` writes `dup` or
    // leaves it untouched on failure, checked via the return value.
    unsafe {
        let current = GetCurrentProcess();
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        let mut dup: HANDLE = std::ptr::null_mut();
        let ok = DuplicateHandle(
            current,
            stdin,
            current,
            &raw mut dup,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_handle(dup.cast()))
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
            let inherit = shell.io.terminal.startup_stdout_tty && group.holds_terminal();
            let plan = shell
                .io
                .stdout
                .child_stdout(inherit)
                .map_err(super::route::pipe_error)?;
            cmd.stdout(plan.stdio);
            Ok(plan.pump)
        }
    }
}

/// Wire a direct external stage's stdin, stdout, and stderr from its route.
pub(super) fn wire_stage_stdio(
    cmd: &mut crate::process::Launch,
    stdin: ByteIn,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &Shell,
) -> Settled<command::ExternalPlumbing> {
    cmd.stdin(route_stdin(stdin, group, shell)?.into_stdio());
    let stdout_pump = wire_stage_stdout(cmd, stdout, group, shell)?;
    let stderr_plan = shell
        .io
        .stderr
        .child_stderr()
        .map_err(super::route::pipe_error)?;
    cmd.stderr(stderr_plan.stdio);
    Ok(command::ExternalPlumbing {
        stdout_pump,
        stderr_pump: stderr_plan.pump,
    })
}

/// Spawn `cmd` into `group` and assemble the [`command::RunningChild`] for a
/// direct external stage.  Its stop reaches the collector through
/// `try_settle`, whatever the policy; `KillAndReap` answers only a stop that
/// somehow reaches `wait` unclaimed.
pub(super) fn spawn_into_group(
    group: &PipelineGroup,
    cmd: &mut crate::process::Launch,
    name: String,
    plumbing: command::ExternalPlumbing,
    mooring: &Mooring,
    shell: &Shell,
    spawn_error: impl FnOnce(std::io::Error) -> Break,
) -> Settled<command::RunningChild> {
    let (child, jail) = group.spawn(cmd).map_err(spawn_error)?;
    let leader = group.leader_pgid();
    if shell.has_active_capabilities() {
        // Windows routes the limits through the pipeline's own job rather
        // than a second per-child one; on Unix `pre_exec` did it already.
        crate::sandbox::apply_child_limits_in_pipeline(&child, leader);
    }
    Ok(command::RunningChild::assemble_with_owner(
        child,
        name,
        plumbing,
        StopPolicy::KillAndReap,
        // Windows group release belongs to `PipelineGroup::drop`.
        command::GroupOwner::BorrowedByPipeline(leader),
        mooring.cancel.as_scope().clone(),
        jail,
    ))
}

pub(super) struct LaunchCx<'a> {
    pub(super) mooring: &'a Mooring,
    pub(super) shell: &'a mut Shell,
    /// The pipeline node's own lexical environment — a thread stage's
    /// captured closure env, distinct from `shell.env` inside a nested
    /// machine (a lambda body, say).
    pub(super) env: &'a Env,
    pub(super) group: &'a mut PipelineGroup,
}

/// Dispatch one stage per its resolve-time [`StageLaunch`] — a direct
/// external spawn, or a ral-written stage on its own thread.
#[allow(
    clippy::needless_pass_by_value,
    reason = "LaunchCx bundles unique `&mut` borrows; by-value transfers them so callees get mutable access — a shared `&LaunchCx` cannot yield `&mut`"
)]
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    mut route: StageRoute,
    cx: LaunchCx<'_>,
    gate: &Arc<StageGate>,
) -> Settled<StageHandle> {
    let held_edge = route.held.take();
    let kind = match &spec.launch {
        StageLaunch::Direct(ext) => StageKind::External(launch_external_stage_direct(
            ext, route, cx.mooring, cx.shell, cx.group,
        )?),
        StageLaunch::Thread => {
            StageKind::Thread(launch_thread_stage(stage, spec, route, cx, gate)?)
        }
    };
    Ok(StageHandle {
        kind,
        held_edge,
        feeds_pipe: spec.feeds_pipe,
        ending: EndingCell::default(),
    })
}

/// Partial-launch resources in teardown order: Rust drops fields top to
/// bottom, and that order is the invariant.  Unconsumed routes close first so
/// half-wired neighbours see EOF, then the stages, then the pgid anchor,
/// which outlives all of them.
struct PipelineResources {
    routes: VecDeque<StageRoute>,
    running: Vec<StageHandle>,
    group: PipelineGroup,
}

impl PipelineResources {
    fn new(group: PipelineGroup, routes: VecDeque<StageRoute>) -> Self {
        Self {
            routes,
            running: Vec::new(),
            group,
        }
    }
}

/// Linear accumulator: one [`PipelineBuild::step`] per stage, then
/// `finish`.  Holding the sole handle to the group and the routes makes a
/// leak a borrow error.  `new` claims the foreground — `tcsetpgrp` — before
/// any stage exists, so no stage runs user code before the kernel's
/// foreground decision is settled.
struct PipelineBuild {
    resources: PipelineResources,
    gate: Arc<StageGate>,
}

impl PipelineBuild {
    fn new(
        mut group: PipelineGroup,
        gate: Arc<StageGate>,
        routes: VecDeque<StageRoute>,
        shell: &Shell,
        mooring: &Mooring,
    ) -> Self {
        group.claim_foreground(shell, mooring);
        Self {
            resources: PipelineResources::new(group, routes),
            gate,
        }
    }

    fn step(
        &mut self,
        stage: &Arc<crate::ir::Comp>,
        spec: &StageSpec,
        env: &Env,
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
            env,
            group: &mut self.resources.group,
        };
        let handle = spawn_stage(stage, spec, route, cx, &self.gate)?;
        self.resources.running.push(handle);
        Ok(())
    }

    /// Tear down a partially-launched pipeline in the one order signal, kill,
    /// join — see `CollectState::cancel_all` — minus the grace: a launch that
    /// failed has no verdict to protect, so nothing is waiting to exit
    /// cleanly.  [`PipelineResources`]'s field order does the rest.
    fn abort(self) {
        let Self { mut resources, .. } = self;
        resources.group.signal(CancelCause::Terminate);
        resources.group.kill();
        drop(resources);
    }

    /// Return the group alongside the running stages — its anchor and
    /// guards must outlive collect.  The foreground was already claimed in
    /// `new`, before any stage existed.
    fn finish(self) -> (PipelineGroup, Vec<StageHandle>) {
        let Self { resources, .. } = self;
        let PipelineResources {
            routes,
            running,
            group,
        } = resources;
        debug_assert!(routes.is_empty());
        (group, running)
    }
}

/// Spawn an external stage with no thread hosting it.  Admitted by
/// `resolve::direct_spawnable` alone: no redirect, no byte-capturing audit.
fn launch_external_stage_direct(
    ext: &ExternalStage,
    route: StageRoute,
    mooring: &Mooring,
    shell: &mut Shell,
    group: &PipelineGroup,
) -> Result<command::RunningChild, Break> {
    let rc = command::vet(&ext.id, &ext.args, shell)?;
    let mut cmd = command::build_command(
        &rc,
        crate::sandbox::Ownership::Kept,
        shell,
        mooring.cancel.as_scope(),
    )?;
    // `spawn_all_stages` polled before this stage; confining it may have taken
    // seconds, so poll again rather than spawn into an expired wall.
    crate::process::check(mooring)?;

    // Nor a redirect, so `ext` carries none and there is no file to open.
    let plumbing = wire_stage_stdio(&mut cmd, route.stdin, route.stdout, group, shell)?;

    // Read before the closure, which cannot borrow what `spawn_into_group` takes
    // mutably.
    let confinement = cmd.confinement();
    spawn_into_group(
        group,
        &mut cmd,
        rc.shown.clone(),
        plumbing,
        mooring,
        shell,
        |e| command::spawn_error(confinement, &rc.shown, &e),
    )
}

/// Spawn every stage, polling for cancellation before each — a prompt abort
/// on a cancel raised between two stages' spawns.
fn spawn_all_stages(
    build: &mut PipelineBuild,
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    for (ix, stage) in stages.iter().enumerate() {
        crate::process::check(mooring)?;
        build.step(stage, &plan.specs[ix], env, mooring, shell)?;
    }
    Ok(())
}

/// Launch every stage into `group`; a mid-launch error goes to
/// [`PipelineBuild::abort`] for the ordered teardown.
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
    group: PipelineGroup,
    gate: &Arc<StageGate>,
) -> Result<(PipelineGroup, Vec<StageHandle>), Break> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(group, Arc::clone(gate), routes, shell, mooring);
    match spawn_all_stages(&mut build, stages, plan, env, mooring, shell) {
        Ok(()) => Ok(build.finish()),
        Err(e) => {
            build.abort();
            Err(e)
        }
    }
}
