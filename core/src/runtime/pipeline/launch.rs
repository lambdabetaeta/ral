//! Process-staged pipeline orchestrator: every stage — a ral-written thread
//! or a direct external — spawns into one process group, joined or owned.
//! [`PipelineBuild`] owns every transient resource — the node under
//! construction and the unconsumed routes — so a leaked pipe end is a borrow
//! error.

use super::super::command;
use super::PipeNode;
use super::collect::{CollectState, Event, Slot, StageEnd, StageObservation};
use super::group::PipelineGroup;
use super::resolve::{
    ExternalStage as ExternalStageSpec, PipelinePlan, StageLaunch, StageSpec, TerminalPlan,
};
use super::route::{ByteIn, ByteOut, HeldEdge, StageRoute, open_stage_routes};
use super::sentinel;
use super::thread::{ThreadStage, launch_thread_stage};
use crate::io::{Sink, Source, SourceReader};
use crate::ir::PipeYield;
use crate::process::CancelCause;
use crate::types::{Break, Env, Error, Mooring, Settled, Shell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// One stage's process — external or thread — paired with the parent's hold
/// on its outbound edge, in the collector's care until this stage's
/// observation completes.
pub(super) struct StageHandle {
    kind: StageKind,
    held: Option<HeldEdge>,
    /// This stage's address on the collector's channel, for the sentinel
    /// [`Self::arm`] starts.
    slot: Slot,
}

/// The collector's handle onto a direct external stage: no dedicated waiter
/// thread — the reaper's own [`crate::process::Watch`] holds the wait and
/// posts the outcome as [`Event::Ended`].
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
    /// sentinel having heard this stage's first write to a dead edge.  Never
    /// reused for any other kill: a thread hears it as the one cause the fold
    /// forgives on the break alone.
    pub(super) fn cut(&mut self) {
        match &mut self.kind {
            StageKind::External(e) => e.watch.kill(),
            StageKind::Thread(t) => {
                t.cancel(CancelCause::ReaderGone);
                t.interrupt();
            }
        }
    }

    /// This stage's wait handle if it is a live external, for the collector's
    /// own per-pid teardown; `None` for a thread stage, which has no pid of
    /// its own to address.
    pub(super) fn watch(&self) -> Option<&crate::process::Watch> {
        match &self.kind {
            StageKind::External(e) => Some(&e.watch),
            StageKind::Thread(_) => None,
        }
    }

    /// Cancel this stage as part of the pipeline's teardown: a thread's scope
    /// is cancelled and the thread woken.  An external is untouched here — a
    /// process hears a cancellation only as a signal, which the collector
    /// sends once, to the group or per pid.
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        if let StageKind::Thread(t) = &mut self.kind {
            t.cancel(cause);
            t.interrupt();
        }
    }

    /// This stage's outbound edge is dead: mark it and hand the read end to
    /// the sentinel.  Marked before the sentinel snapshots what is pending,
    /// so a write completing between the two is caught by the sink's own
    /// post-check rather than lost.
    pub(super) fn arm(&mut self) {
        if let Some(held) = &mut self.held
            && let Some(reader) = held.reader.take()
        {
            held.edge.mark_dead();
            sentinel::listen(reader, self.slot.clone());
        }
    }

    /// The sentinel's read end, back from the news it carried.
    pub(super) fn regain(&mut self, reader: os_pipe::PipeReader) {
        if let Some(held) = &mut self.held {
            held.reader = Some(reader);
        }
    }

    /// An external's own terminal event: reap the watch — only once this
    /// stage has left the collector's `stages`, so no kill can ever name a
    /// reaped pid — then release the held-open read end, freeing any
    /// descendant still blocked writing into it.  The jail and the pumps both
    /// wait on `cancel_all`'s group kill, so they are the fold's to finish
    /// and this stays non-blocking.
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
    /// arrived, so this reclaims the join rather than waiting for it, then
    /// releases the held-open read end.
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
    /// own tests: the wiring `spawn_stage` does, without a pipeline launch.
    #[cfg(test)]
    pub(super) fn for_test(slot: Slot, child: crate::process::ChildHandle) -> Self {
        Self {
            kind: StageKind::External(ExternalStage {
                watch: slot.watch(child),
                name: "test".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
            }),
            held: None,
            slot,
        }
    }

    /// A `StageHandle` around a real child nobody watches for, for `step`'s
    /// own transition-table tests: they synthesize the events, so all this
    /// owes them is `cut`'s dispatch and an already-dead pid to reap.
    #[cfg(test)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:test] test process scaffolding"
    )]
    pub(super) fn fake_external_for_step_test(slot: Slot) -> Self {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a fake stage's child");
        let _ = child.kill();
        // A throwaway channel: this pid's real exit must not reach the
        // collector, whose own `Ended` the test synthesizes by hand.
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch =
            crate::process::ChildHandle::from_std(child).into_watch(tx, std::convert::identity);
        Self {
            kind: StageKind::External(ExternalStage {
                watch,
                name: "fake".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
            }),
            held: None,
            slot,
        }
    }

    /// A `StageHandle` around no real thread, so an `Event::Returned` for it
    /// files as a real thread stage's own end would.
    #[cfg(test)]
    pub(super) fn fake_thread_for_step_test(slot: Slot) -> Self {
        Self {
            kind: StageKind::Thread(ThreadStage::fake_for_step_test()),
            held: None,
            slot,
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
    let reader = shell.io.stdin.reader().map_err(stdin_error)?;
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

fn stdin_error(e: impl std::fmt::Display) -> Break {
    Break::Error(Error::new(format!("could not duplicate stdin: {e}"), 1))
}

/// A thread reads what a child would inherit, except a tty: reading the
/// controlling terminal from the shell's own process would SIGTTIN the whole
/// shell, so it sees EOF, as under capture.  The thread's own wake ends a
/// blocked read as EOF.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    wake: &Arc<crate::process::Wake>,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => Some(SourceReader::pipe(r)),
        ByteIn::Parent => match &shell.io.stdin {
            Source::Empty => None,
            Source::Terminal if shell.io.terminal.startup_stdin_tty => None,
            Source::Terminal => Some(SourceReader::file(dup_stdin_file().map_err(stdin_error)?)),
            Source::Reader(r) => Some(r.try_clone().map_err(stdin_error)?),
        },
    };
    Ok(reader.map_or(Source::Empty, |r| {
        Source::Reader(r.interruptible(Arc::clone(wake)))
    }))
}

#[cfg(unix)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

#[cfg(windows)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle;
    std::io::stdin()
        .as_handle()
        .try_clone_to_owned()
        .map(std::fs::File::from)
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
fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    mut route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: Slot,
    is_last: bool,
) -> Settled<StageHandle> {
    let held = route.held.take();
    let kind = match &spec.launch {
        StageLaunch::Direct(ext) => {
            StageKind::External(launch_external_stage_direct(ext, route, cx, &slot)?)
        }
        StageLaunch::Thread => StageKind::Thread(launch_thread_stage(
            stage,
            spec,
            route,
            cx,
            slot.clone(),
            is_last,
        )?),
    };
    Ok(StageHandle { kind, held, slot })
}

/// Everything [`super::PipeNode::launch`] settles before any stage exists,
/// handed to the launcher whole.
pub(super) struct PipelineStart {
    pub(super) group: PipelineGroup,
    pub(super) yields: PipeYield,
    pub(super) tx: Sender<Event>,
    pub(super) rx: Receiver<Event>,
    /// Window start for the sandbox-denial reader.
    pub(super) started: Instant,
}

/// Linear accumulator: one [`PipelineBuild::step`] per stage, then `finish`.
/// Holding the sole handle to the node and the routes makes a leak a borrow
/// error.  `new` claims the foreground before any stage exists, so no stage
/// runs before the kernel's foreground decision is settled.
///
/// Field order is teardown order: unconsumed routes close first so half-wired
/// neighbours see EOF, then the node, whose own field order puts the
/// collector before the anchor it must outlive.
struct PipelineBuild {
    routes: VecDeque<StageRoute>,
    node: PipeNode,
}

impl PipelineBuild {
    fn new(
        start: PipelineStart,
        terminal: TerminalPlan,
        routes: VecDeque<StageRoute>,
        shell: &Shell,
        mooring: &Mooring,
    ) -> Self {
        let PipelineStart {
            mut group,
            yields,
            tx,
            rx,
            started,
        } = start;
        if matches!(terminal, TerminalPlan::ForegroundExternalGroup) {
            group.claim_foreground(shell, mooring);
        }
        let collect = CollectState::new(rx, tx, &group, mooring, started);
        Self {
            routes,
            node: PipeNode {
                collect,
                group,
                yields,
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
        let route = self.routes.pop_front().expect("one route per stage");
        let slot = self.node.collect.slot();
        let mut cx = LaunchCx {
            mooring,
            shell,
            env,
            group: &mut self.node.group,
        };
        let handle = spawn_stage(stage, spec, route, &mut cx, slot, is_last)?;
        self.node.collect.push(handle);
        Ok(())
    }

    fn finish(mut self) -> PipeNode {
        self.node.collect.all_stages_launched();
        let Self { routes, node } = self;
        debug_assert!(routes.is_empty());
        node
    }
}

/// Spawn an external stage with no thread hosting it — `resolve_launch`
/// admits only a redirect-free stage under no byte-capturing audit, so `ext`
/// carries no redirect and there is no file to open.
fn launch_external_stage_direct(
    ext: &ExternalStageSpec,
    route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<ExternalStage> {
    let rc = command::vet(&ext.id, &ext.args, cx.shell)?;
    let mut cmd = command::build_command(
        &rc,
        crate::sandbox::Ownership::Kept,
        cx.shell,
        cx.mooring.cancel.as_scope(),
    )?;
    // Confinement may have taken seconds since the caller's own poll, so poll
    // again rather than spawn into an expired wall.
    crate::process::check(cx.mooring)?;

    let plumbing = wire_stage_stdio(&mut cmd, route.stdin, route.stdout, cx.group, cx.shell)?;

    // Read before the spawn, which takes `cmd` mutably.
    let confinement = cmd.confinement();
    let (mut child, jail) = cx
        .group
        .spawn(&mut cmd)
        .map_err(|e| command::spawn_error(confinement, &rc.shown, &e))?;
    if cx.shell.has_active_capabilities() {
        // Windows routes the limits through the pipeline's own job rather
        // than a second per-child one; on Unix `pre_exec` did it already.
        crate::sandbox::apply_child_limits_in_pipeline(&child, cx.group.leader_pgid());
    }
    let pumps = command::Pumps::spawn(plumbing, &mut child);
    Ok(ExternalStage {
        watch: slot.watch(child),
        name: rc.shown,
        jail,
        pumps,
    })
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

/// Launch every stage into the started group.  A mid-launch error drops
/// `build`, whose field order is the teardown.
pub(super) fn launch_pipeline(
    stages: &[Arc<crate::ir::Comp>],
    plan: &PipelinePlan,
    start: PipelineStart,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<PipeNode> {
    let routes = open_stage_routes(plan)?.into();
    let mut build = PipelineBuild::new(start, plan.terminal, routes, shell, mooring);
    spawn_all_stages(&mut build, stages, plan, env, mooring, shell)?;
    Ok(build.finish())
}
