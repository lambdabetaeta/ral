//! Process-staged pipeline orchestrator: every stage — a ral-written thread
//! or a direct external — spawns into one process group, joined or owned.
//! [`PipelineBuild`] owns every transient resource — group, collector,
//! unconsumed routes — so a leaked pipe end is a borrow error.

use super::super::command;
use super::collect::{CollectState, Event, StageEnd, StageObservation};
use super::group::PipelineGroup;
use super::resolve::{
    ExternalStage as ExternalStageSpec, PipelinePlan, StageLaunch, StageSpec, TerminalPlan,
};
use super::route::{ByteIn, ByteOut, HeldEdge, StageRoute, open_stage_routes};
use super::sentinel;
use super::thread::{ThreadStage, launch_thread_stage};
use crate::io::{Sink, Source, SourceReader};
use crate::process::CancelCause;
use crate::types::{Break, Env, Error, Mooring, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::Sender;

/// One stage's process — external or thread — paired with the parent's hold
/// on its outbound edge, in the collector's care until this stage's
/// observation completes.
pub(super) struct StageHandle {
    kind: StageKind,
    held: Option<HeldEdge>,
    /// The collector's channel, for the sentinel [`Self::arm`] starts.
    report: Sender<Event>,
}

/// The collector's handle onto a direct external stage: no dedicated waiter
/// thread — the reaper's own [`crate::process::Watch`] owns this child's
/// wait, and `into_watch`'s closure posts its outcome straight onto the
/// collector's channel as [`Event::Ended`].
struct ExternalStage {
    watch: crate::process::Watch,
    name: String,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.
    jail: Option<crate::process::jail::JailCgroup>,
    pumps: command::Pumps,
}

enum StageKind {
    External(ExternalStage),
    Thread(ThreadStage),
}

impl StageHandle {
    /// [`super::collect::Effect::KillStage`]'s mechanical action, the
    /// sentinel having heard this stage's first write to a dead edge.  The
    /// attribution — the collector's own `sent` — is `step`'s.
    pub(super) fn kill_now(&mut self) {
        match &mut self.kind {
            StageKind::External(e) => e.watch.kill(),
            StageKind::Thread(t) => {
                t.cancel(CancelCause::ReaderGone);
                t.interrupt();
            }
        }
    }

    /// Kill this stage's pid alone if it is a live external — a joining
    /// collector's `cancel_all`, with no pgid of its own to kill, reaching
    /// what it launched directly.  A no-op for a thread stage, already
    /// cancelled and woken by [`Self::cancel`].
    pub(super) fn kill_by_pid(&self) {
        if let StageKind::External(e) = &self.kind {
            e.watch.kill();
        }
    }

    /// End this stage as part of the pipeline's teardown; the caller signals
    /// and kills the group separately.  A thread is also woken, which ends
    /// the read or write it is blocked in.  The cause signal is delivered to
    /// an external's pid directly — cancellation reaches what this collector
    /// launched whether or not it owns a group to signal; a stage's own kill
    /// reaches its pid alone, and descendants are the group owner's to end.
    /// For an owned group this duplicates the group's own signal on the same
    /// pid, which is harmless.  Windows has no non-lethal signal, so an
    /// external stage's own `cancel` is a no-op there — the group's own
    /// eventual kill finishes it off.
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        match &mut self.kind {
            StageKind::External(e) => {
                #[cfg(unix)]
                e.watch
                    .signal(crate::process::Signal::new(crate::process::cause_signal(cause)));
                #[cfg(windows)]
                let _ = (e, cause);
            }
            StageKind::Thread(t) => {
                t.cancel(cause);
                t.interrupt();
            }
        }
    }

    /// This stage's outbound edge is dead: mark it and hand the read end to
    /// the sentinel.  Marked before the sentinel snapshots what is pending,
    /// so a write completing between the two is caught by the sink's own
    /// post-check rather than lost.
    pub(super) fn arm(&mut self, ix: usize) {
        if let Some(held) = &mut self.held
            && let Some(reader) = held.reader.take()
        {
            held.edge.mark_dead();
            sentinel::listen(reader, ix, self.report.clone());
        }
    }

    /// The sentinel's read end, back from the news it carried.
    pub(super) fn regain(&mut self, reader: os_pipe::PipeReader) {
        if let Some(held) = &mut self.held {
            held.reader = Some(reader);
        }
    }

    /// An external's own terminal event: reap the watch — after this stage
    /// has already left the collector's `stages`, so no `KillStage` can ever
    /// name a reaped pid — then release the held-open read end, whether the
    /// sentinel returned it or never took it, only now that the writer is
    /// reaped so any descendant of that edge still blocked writing into it is
    /// freed.  Neither the jail nor the pumps are settled here: the jail's
    /// `rmdir` polls while descendants are still dying, and a descendant that
    /// survived the stage's pid-addressed kill still holds the pipe a pump
    /// reads — both wait on `cancel_all`'s group kill, so both are the fold's
    /// to finish, after the walk, and this stays non-blocking.
    pub(super) fn file_external_end(self, outcome: crate::process::WaitOutcome) -> StageEnd {
        let Self { held, kind, .. } = self;
        let StageKind::External(e) = kind else {
            panic!("Event::Ended named a stage that was not spawned as an external");
        };
        let _ = e.watch.reap();
        drop(held);
        StageEnd::External {
            name: e.name,
            outcome,
            jail: e.jail,
            pumps: e.pumps,
        }
    }

    /// A thread stage's own terminal event: its `Returned` has already
    /// arrived by channel, so this reclaims the join rather than waiting for
    /// it, then releases the held-open read end, whether the sentinel
    /// returned it or never took it.
    pub(super) fn file_thread_end(self, obs: StageObservation) -> StageEnd {
        let Self { held, kind, .. } = self;
        let StageKind::Thread(t) = kind else {
            panic!("Event::Returned named a stage that was not a thread stage");
        };
        t.join_after_settled();
        drop(held);
        StageEnd::Thread(obs)
    }

    /// A `StageHandle` around an already-running external, for `collect.rs`'s
    /// own tests: wires it into `collect`'s channel exactly as `spawn_stage`
    /// does, without a whole pipeline launch to set one up.
    #[cfg(test)]
    pub(super) fn for_test(collect: &CollectState, child: crate::process::ChildHandle) -> Self {
        let ix = collect.next_index();
        let watch = child.into_watch(collect.sender(), move |o| Event::Ended(ix, o));
        Self {
            kind: StageKind::External(ExternalStage {
                watch,
                name: "test".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
            }),
            held: None,
            report: collect.sender(),
        }
    }

    /// A `StageHandle` around a real child nobody watches for, for `step`'s
    /// own transition-table tests: they drive events by hand rather than
    /// waiting on a real exit, so all this needs to support is `kill_now`'s
    /// mechanical dispatch and, once a test synthesizes this stage's own
    /// `Event::Ended`, a real (already-dead) pid for `Watch::reap` to
    /// consume.  Spawned and killed at once, as the reaper's own tests do.
    #[cfg(test)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:test] test process scaffolding"
    )]
    pub(super) fn fake_external_for_step_test() -> Self {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a fake stage's child");
        let _ = child.kill();
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = crate::process::ChildHandle::from_std(child)
            .into_watch(tx, std::convert::identity);
        Self {
            kind: StageKind::External(ExternalStage {
                watch,
                name: "fake".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
            }),
            held: None,
            report: std::sync::mpsc::channel().0,
        }
    }

    /// A `StageHandle` around no real thread, for `collect.rs`'s own
    /// `SettleOnDrop` test: a thread-kind fake, so an `Event::Returned` for
    /// it is the shape a real thread stage's own end would be.
    #[cfg(test)]
    pub(super) fn fake_thread_for_step_test() -> Self {
        Self {
            kind: StageKind::Thread(ThreadStage::fake_for_step_test()),
            held: None,
            report: std::sync::mpsc::channel().0,
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
        // A process's writes are heard by the sentinel, so the edge's fate is
        // none of this wiring's business.
        ByteOut::Downstream(writer, _edge) => {
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

/// Spawn `cmd` into `group`, wire its pumps, and watch it: the sole assembly
/// point for a direct external stage.
#[allow(
    clippy::too_many_arguments,
    reason = "the single assembly point for an external stage; splitting it would just scatter the same parameters across a builder"
)]
fn spawn_into_group(
    group: &PipelineGroup,
    cmd: &mut crate::process::Launch,
    name: String,
    plumbing: command::ExternalPlumbing,
    shell: &Shell,
    ix: usize,
    tx: Sender<Event>,
    spawn_error: impl FnOnce(std::io::Error) -> Break,
) -> Settled<ExternalStage> {
    let (mut child, jail) = group.spawn(cmd).map_err(spawn_error)?;
    let leader = group.leader_pgid();
    if shell.has_active_capabilities() {
        // Windows routes the limits through the pipeline's own job rather
        // than a second per-child one; on Unix `pre_exec` did it already.
        crate::sandbox::apply_child_limits_in_pipeline(&child, leader);
    }
    let pumps = command::Pumps::spawn(plumbing, &mut child);
    let watch = child.into_watch(tx, move |o| Event::Ended(ix, o));
    Ok(ExternalStage {
        watch,
        name,
        jail,
        pumps,
    })
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
#[allow(
    clippy::too_many_arguments,
    reason = "one dispatch call per stage; a thread stage's index, finality, and sender have nowhere else to ride"
)]
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    mut route: StageRoute,
    cx: LaunchCx<'_>,
    ix: usize,
    is_last: bool,
    tx: Sender<Event>,
) -> Settled<StageHandle> {
    let held = route.held.take();
    let report = tx.clone();
    let kind = match &spec.launch {
        StageLaunch::Direct(ext) => {
            let stage =
                launch_external_stage_direct(ext, route, cx.mooring, cx.shell, cx.group, ix, tx)?;
            StageKind::External(stage)
        }
        StageLaunch::Thread => StageKind::Thread(launch_thread_stage(
            stage, spec, route, cx, ix, is_last, tx,
        )?),
    };
    Ok(StageHandle { kind, held, report })
}

/// Partial-launch resources in teardown order: Rust drops fields top to
/// bottom, and that order is the invariant.  Unconsumed routes close first so
/// half-wired neighbours see EOF; then the collector, whose drop kills the
/// group before its handles join; then the pgid anchor, which outlives all
/// of them.
struct PipelineResources {
    routes: VecDeque<StageRoute>,
    collect: CollectState,
    group: PipelineGroup,
}

/// Linear accumulator: one [`PipelineBuild::step`] per stage, then
/// `finish`.  Holding the sole handle to the group and the routes makes a
/// leak a borrow error.  `new` claims the foreground — `tcsetpgrp` — before
/// any stage exists, so no stage runs user code before the kernel's
/// foreground decision is settled.
struct PipelineBuild {
    resources: PipelineResources,
}

impl PipelineBuild {
    fn new(
        mut group: PipelineGroup,
        terminal: TerminalPlan,
        routes: VecDeque<StageRoute>,
        shell: &Shell,
        mooring: &Mooring,
        started: std::time::Instant,
    ) -> Self {
        if matches!(terminal, TerminalPlan::ForegroundExternalGroup) {
            group.claim_foreground(shell, mooring);
        }
        let collect = CollectState::new(&mut group, mooring, started);
        Self {
            resources: PipelineResources {
                routes,
                collect,
                group,
            },
        }
    }

    fn step(
        &mut self,
        stage: &Arc<crate::ir::Comp>,
        spec: &StageSpec,
        is_last: bool,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        let route = self
            .resources
            .routes
            .pop_front()
            .expect("one route per stage");
        let ix = self.resources.collect.next_index();
        let tx = self.resources.collect.sender();
        let cx = LaunchCx {
            mooring,
            shell,
            env,
            group: &mut self.resources.group,
        };
        let handle = spawn_stage(stage, spec, route, cx, ix, is_last, tx)?;
        self.resources.collect.push(handle);
        Ok(())
    }

    /// Return the group alongside the collector — its anchor and guards must
    /// outlive collect.  The foreground was already claimed in `new`, before
    /// any stage existed.
    fn finish(mut self) -> (PipelineGroup, CollectState) {
        self.resources.collect.all_stages_launched();
        let Self { resources, .. } = self;
        let PipelineResources {
            routes,
            collect,
            group,
        } = resources;
        debug_assert!(routes.is_empty());
        (group, collect)
    }
}

/// Spawn an external stage with no thread hosting it.  Admitted by
/// `resolve::direct_spawnable` alone: no redirect, no byte-capturing audit.
#[allow(
    clippy::too_many_arguments,
    reason = "one dispatch call per external stage; ix and tx have nowhere else to ride"
)]
fn launch_external_stage_direct(
    ext: &ExternalStageSpec,
    route: StageRoute,
    mooring: &Mooring,
    shell: &mut Shell,
    group: &PipelineGroup,
    ix: usize,
    tx: Sender<Event>,
) -> Result<ExternalStage, Break> {
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
        shell,
        ix,
        tx,
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
    let n = stages.len();
    for (ix, stage) in stages.iter().enumerate() {
        crate::process::check(mooring)?;
        build.step(stage, &plan.specs[ix], ix + 1 == n, env, mooring, shell)?;
    }
    Ok(())
}

/// Launch every stage into `group`.  A mid-launch error drops `build`, whose
/// field order is the teardown.
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
    group: PipelineGroup,
    started: std::time::Instant,
) -> Result<(PipelineGroup, CollectState), Break> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(group, plan.terminal, routes, shell, mooring, started);
    spawn_all_stages(&mut build, stages, plan, env, mooring, shell)?;
    Ok(build.finish())
}
