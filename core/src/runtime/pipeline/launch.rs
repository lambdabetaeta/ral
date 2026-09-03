//! Process-staged pipeline orchestrator: every stage — a ral-written thread
//! or a direct external — spawns into one process group, joined or owned.
//! [`PipelineBuild`] owns every transient resource — group, collector,
//! unconsumed routes — so a leaked pipe end is a borrow error.

use super::super::command;
use super::collect::{CollectState, Report, SettleOnDrop, Settlement, StageObservation};
use super::group::PipelineGroup;
use super::resolve::{ExternalStage as ExternalStageSpec, PipelinePlan, StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, StageRoute, open_stage_routes};
use super::thread::{ThreadStage, launch_thread_stage};
use crate::io::{Sink, Source, SourceReader};
use crate::process::{CancelCause, CancelScope, Ending, StageGate, StopPolicy};
use crate::types::{Break, Env, Error, Mooring, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::Sender;

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
}

/// The collector's handle onto a direct external stage's own dedicated
/// waiter thread — the sole owner of that child's wait from spawn to
/// terminal event.  Everything else the collector once read off the child
/// directly (`try_settle`, a stop level) is now the waiter's own business,
/// reported as events; only the pid (a raw signal target) and `kill_cause`
/// still cross back to this side.
///
/// `kill_cause` is its own fresh scope, never the mooring's: the mooring's
/// cancel scope is shared with every sibling stage (and beyond), so raising
/// `ReaderGone` on it to attribute *this* stage's own kill would cancel them
/// too.  [`command::RunningChild::run_pipeline_stage`] reads it instead of
/// the `RunningChild`'s own `cancel` field, which a pipeline external's
/// waiter never consults.
struct ExternalWaiter {
    join: Option<std::thread::JoinHandle<()>>,
    kill_cause: CancelScope,
    pid: u32,
}

enum StageKind {
    External(ExternalWaiter),
    Thread(ThreadStage),
}

/// Spawn the dedicated waiter thread that owns `running`'s wait exclusively:
/// a blocking run to the stage's own end, answering every stop it sees
/// inline, then `Settled` — never a probe the collector calls into.
fn spawn_external_waiter(running: command::RunningChild, ix: usize, tx: Sender<Report>) -> ExternalWaiter {
    let kill_cause = CancelScope::root();
    let pid = running
        .child
        .as_ref()
        .expect("a freshly spawned RunningChild holds its child")
        .id();
    let waiter_kill_cause = kill_cause.clone();
    let join = std::thread::Builder::new()
        .name("ral pipeline external waiter".to_string())
        .spawn(move || {
            let settle = SettleOnDrop::new(ix, tx);
            let (name, failure) = running.run_pipeline_stage(&waiter_kill_cause);
            settle.send(Settlement::External { name, failure });
        })
        .expect("spawn the pipeline's external waiter thread");
    ExternalWaiter {
        join: Some(join),
        kill_cause,
        pid,
    }
}

impl StageHandle {
    /// [`super::collect::Effect::KillStage`]'s mechanical action: the
    /// reader-gone cascade alone, whose guard — this stage still running
    /// and feeding the pipe — `step` has already checked.  The **one**
    /// place that raises the forgiven ending on either record: a thread's
    /// `ThreadStage::ending`, or an external's `kill_cause`.  No other kill
    /// on this stage may claim a death nothing sent it.  See
    /// [`Self::kill_stopped`] for the other kill, which must not.
    pub(super) fn kill_now(&mut self) {
        match &mut self.kind {
            StageKind::External(e) => {
                e.kill_cause.cancel(CancelCause::ReaderGone);
                crate::process::kill_stage_by_pid(e.pid);
            }
            StageKind::Thread(t) => {
                t.cancel(CancelCause::ReaderGone);
                t.interrupt();
            }
        }
    }

    /// [`super::collect::Effect::KillStoppedStage`]'s mechanical action: a
    /// background group's stop-then-kill, fired before the group's own
    /// `SIGCONT` could wake the child.  Only ever reached for an external.
    /// Raises nothing on either record — the subsequent `CancelAll(Terminate)`
    /// still stamps this stage's `Ending` via its own `cancel_stages`.
    #[cfg(unix)]
    pub(super) fn kill_stopped(&self) {
        self.kill_by_pid();
    }

    /// Kill this stage's pid alone if it is a live external — a joining
    /// collector's `cancel_all`, with no pgid of its own to kill, reaching
    /// what it launched directly.  Raises nothing on either record, like
    /// [`Self::kill_stopped`]: a thread stage was already cancelled and
    /// woken by [`Self::cancel`].
    pub(super) fn kill_by_pid(&self) {
        if let StageKind::External(e) = &self.kind {
            crate::process::kill_stage_by_pid(e.pid);
        }
    }

    /// End this stage as part of the pipeline's teardown; the caller signals
    /// and kills the group separately.  A thread is also woken, which ends
    /// the read or write it is blocked in.  An external's `kill_cause` is
    /// set for its waiter's ending attribution, and the cause signal is
    /// delivered to its pid directly — cancellation reaches what this
    /// collector launched whether or not it owns a group to signal; a
    /// stage's own kill reaches its pid alone, and descendants are the group
    /// owner's to end.  For an owned group this duplicates the group's own
    /// signal on the same pid, which is harmless.
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        match &mut self.kind {
            StageKind::External(e) => {
                e.kill_cause.cancel(cause);
                #[cfg(unix)]
                crate::process::signal_stage_by_pid(e.pid, cause);
            }
            StageKind::Thread(t) => {
                t.cancel(cause);
                t.interrupt();
            }
        }
    }

    /// Whether this is a thread stage: `step`'s only use of the kind that
    /// isn't itself dispatched by a [`StageHandle`] method — the reader-gone
    /// cascade skips a background group's stop-then-kill for a thread, whose
    /// own settlement, via `CancelAll`, already covers it.  Unused now that
    /// nothing in `step` names it.
    #[allow(dead_code)]
    pub(super) fn is_thread(&self) -> bool {
        matches!(self.kind, StageKind::Thread(_))
    }

    /// Whether this stage's stdout can still reach the interior edge — the
    /// reader-gone cascade's other half of its guard, mirroring
    /// `StageSpec::feeds_pipe`.
    pub(super) fn feeds_pipe(&self) -> bool {
        self.feeds_pipe
    }

    /// Reduce this stage's own `Settled` event to its final observation, then
    /// release the held-open read end — only now that the writer is reaped,
    /// so any descendant of that edge still blocked writing into it is freed.
    /// Total over both kinds, not a match with an `External` no-op standing
    /// in for "nothing left to do": an external's `obs` already carries
    /// everything (`CollectState::resolve` built it, `&Shell` and all), so
    /// only its waiter thread wants reclaiming; a thread's `Break` carries no
    /// mark of whether the kill or its own code ended it, so a killed one is
    /// forgiven whatever it returned.
    pub(super) fn file_settled(self, obs: StageObservation) -> StageObservation {
        let Self { held_edge, kind, .. } = self;
        let obs = match kind {
            StageKind::External(e) => {
                if let Some(join) = e.join {
                    let _ = join.join();
                }
                obs
            }
            StageKind::Thread(t) => {
                let ending = t.ending();
                t.join_after_settled();
                match ending {
                    Ending::RalEnded(CancelCause::ReaderGone) => obs.forgiven(),
                    _ => obs,
                }
            }
        };
        drop(held_edge);
        obs
    }

    /// A `StageHandle` around an already-running external, for `collect.rs`'s
    /// own tests: wires its own dedicated waiter thread into `collect`'s
    /// channel exactly as `spawn_stage` does, without a whole pipeline launch
    /// to set one up.
    #[cfg(test)]
    pub(super) fn for_test(collect: &CollectState, child: command::RunningChild) -> Self {
        let ix = collect.next_index();
        let waiter = spawn_external_waiter(child, ix, collect.sender());
        Self {
            kind: StageKind::External(waiter),
            held_edge: None,
            feeds_pipe: true,
        }
    }

    /// A `StageHandle` around no process at all, for `step`'s own
    /// transition-table tests: they drive events by hand, so all this needs
    /// to support is `kill_now`'s mechanical dispatch — which lands on a pid
    /// nothing ever spawned, an `ESRCH` no different from signalling a
    /// process that already exited.
    #[cfg(test)]
    pub(super) fn fake_external_for_step_test() -> Self {
        Self {
            kind: StageKind::External(ExternalWaiter {
                join: None,
                kill_cause: CancelScope::root(),
                // Comfortably past any real pid on every platform ral runs
                // on, and never `-1`/`0`, which `kill` reads as "every
                // process in a group" rather than "no such process".
                pid: i32::MAX as u32,
            }),
            held_edge: None,
            feeds_pipe: true,
        }
    }

    /// This stage's waiter's own `kill_cause`, for `step`'s transition-table
    /// tests to assert on directly — whether a kill attributed itself there.
    /// Its only caller compares `kill_now` against `kill_stopped`, which is
    /// itself `cfg(unix)`; unreachable for a thread, which this test never
    /// builds.
    #[cfg(all(test, unix))]
    pub(super) fn kill_cause_for_test(&self) -> Option<CancelCause> {
        match &self.kind {
            StageKind::External(e) => e.kill_cause.cause(),
            StageKind::Thread(_) => unreachable!("the test only builds externals"),
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
/// direct external stage.  `StopPolicy::KillAndReap` is inert here — its own
/// dedicated waiter thread ([`spawn_external_waiter`]) never consults it,
/// reporting every stop and answering none — but `RunningChild` carries no
/// "no policy" shape, so the field must still hold something.
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
        command::GroupOwner::BorrowedByPipeline,
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
#[allow(
    clippy::too_many_arguments,
    reason = "one dispatch call per stage; a thread stage's index, finality, and sender have nowhere else to ride"
)]
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    mut route: StageRoute,
    cx: LaunchCx<'_>,
    gate: &Arc<StageGate>,
    ix: usize,
    is_last: bool,
    tx: Sender<Report>,
) -> Settled<StageHandle> {
    let held_edge = route.held.take();
    let kind = match &spec.launch {
        StageLaunch::Direct(ext) => {
            let running =
                launch_external_stage_direct(ext, route, cx.mooring, cx.shell, cx.group)?;
            StageKind::External(spawn_external_waiter(running, ix, tx))
        }
        StageLaunch::Thread => StageKind::Thread(launch_thread_stage(
            stage, spec, route, cx, gate, ix, is_last, tx,
        )?),
    };
    Ok(StageHandle {
        kind,
        held_edge,
        feeds_pipe: spec.feeds_pipe,
    })
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
    gate: Arc<StageGate>,
}

impl PipelineBuild {
    fn new(
        mut group: PipelineGroup,
        gate: Arc<StageGate>,
        routes: VecDeque<StageRoute>,
        shell: &Shell,
        mooring: &Mooring,
        started: std::time::Instant,
    ) -> Self {
        group.claim_foreground(shell, mooring);
        let collect = CollectState::new(&mut group, mooring, started);
        Self {
            resources: PipelineResources {
                routes,
                collect,
                group,
            },
            gate,
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
        let handle = spawn_stage(stage, spec, route, cx, &self.gate, ix, is_last, tx)?;
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
fn launch_external_stage_direct(
    ext: &ExternalStageSpec,
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
    let n = stages.len();
    for (ix, stage) in stages.iter().enumerate() {
        crate::process::check(mooring)?;
        build.step(stage, &plan.specs[ix], ix + 1 == n, env, mooring, shell)?;
    }
    Ok(())
}

/// Launch every stage into `group`.  A mid-launch error drops `build`, whose
/// field order is the teardown.
#[allow(
    clippy::too_many_arguments,
    reason = "one launch call per pipeline; splitting it would just scatter the same parameters across a builder"
)]
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
    group: PipelineGroup,
    gate: &Arc<StageGate>,
    started: std::time::Instant,
) -> Result<(PipelineGroup, CollectState), Break> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(group, Arc::clone(gate), routes, shell, mooring, started);
    spawn_all_stages(&mut build, stages, plan, env, mooring, shell)?;
    Ok(build.finish())
}
