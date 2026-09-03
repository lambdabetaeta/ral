//! A stage thread: a ral-written pipeline stage evaluated on its own OS
//! thread, sharing the pipeline's process group with whatever externals it
//! spawns.  [`launch_thread_stage`] is the parent side — it wires the
//! stage's `Io` from its [`StageRoute`] and hands the closure to
//! [`Shell::spawn_thread`]; [`ThreadStage`] is the collector's handle onto
//! the running thread.

use super::launch::LaunchCx;
use super::resolve::StageSpec;
use super::route::{ByteOut, StageRoute};
use crate::evaluator::machine;
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::source::Span;
use crate::types::{Break, Closure, Error, Mooring, Settled};
use crate::process::{
    CancelCause, CancelScope, Ending, EndingCell, StageGate, StagePark, StageStop, Wake,
};
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    /// Reports this stage's own interior stop — a child it spawned itself
    /// stopping — as `Report::Stopped`/`Report::Continued`, one edge at a
    /// time, off [`StageStop`]'s condvar rather than a cell the collector
    /// must poll.  `None` on Windows: nothing stops there.
    #[cfg(unix)]
    watcher: Option<std::thread::JoinHandle<()>>,
    stop: Arc<StageStop>,
    cancel: CancelScope,
    wake: Arc<Wake>,
    /// Private to this module: only `cancel` (via `StageHandle::kill_now`
    /// and `StageHandle::cancel`) raises this, and `file_settled` is its only
    /// reader.
    ending: EndingCell,
}

impl ThreadStage {
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        self.ending.raise(Ending::RalEnded(cause));
        self.cancel.cancel(cause);
    }

    /// This stage's own `Ending`, read by `file_settled` alone: whether a
    /// kill raised the forgiven one.
    pub(super) fn ending(&self) -> Ending {
        self.ending.get()
    }

    /// Fire the wake; on Windows also `CancelSynchronousIo` the stage
    /// thread's blocked `ReadFile`, retried until it acknowledges the wake
    /// or the stage has already finished on its own.
    pub(super) fn interrupt(&self) {
        self.wake.fire();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::IO::CancelSynchronousIo;
            let Some(join) = &self.join else { return };
            while !self.wake.acknowledged() && !join.is_finished() {
                // SAFETY: `join`'s handle is valid for the thread's whole
                // life, which this `ThreadStage` outlives by construction.
                unsafe { CancelSynchronousIo(join.as_raw_handle().cast()) };
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// This stage's `Settled` event has already arrived by channel — the
    /// thread is already returning, so this reclaims it rather than waiting
    /// for it.  The watcher closed itself the moment `StageStop::close` ran,
    /// the stage's own last act before that same `Settled` was sent, so its
    /// join is just as immediate.
    pub(super) fn join_after_settled(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        #[cfg(unix)]
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Turn a caught panic's payload into the `Error` a stage's own `Settled`
/// carries: downcast the usual `&str`/`String` shapes, else name it unknown,
/// and stamp the stage's own span, since the panicking thread's stack
/// carries none of its own to attribute it to.
fn panic_error(payload: &(dyn std::any::Any + Send), span: Option<Span>) -> Error {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    let mut err = Error::new(format!("ral pipeline stage panicked: {msg}"), 1);
    err.span = span;
    err
}

impl Drop for ThreadStage {
    /// A stage never observed — an aborted launch, a panic elsewhere in the
    /// pipeline unwinding past it — is cancelled, interrupted, and joined
    /// rather than abandoned.  The watcher is closed and joined alongside:
    /// otherwise it would outlive the pipeline, blocked on an edge that will
    /// never come.
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel(CancelCause::Terminate);
        self.interrupt();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.stop.close();
        #[cfg(unix)]
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Spawn one ral-written stage on its own thread.
#[allow(
    clippy::needless_pass_by_value,
    reason = "LaunchCx bundles unique `&mut` borrows; by-value transfers them so this fn gets mutable access — a shared `&LaunchCx` cannot yield `&mut`"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "one launch call per stage; its index, finality, and sender have nowhere else to ride"
)]
pub(super) fn launch_thread_stage(
    stage: &Arc<Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: LaunchCx<'_>,
    gate: &Arc<StageGate>,
    ix: usize,
    is_last: bool,
    tx: std::sync::mpsc::Sender<super::collect::Report>,
) -> Settled<ThreadStage> {
    let wake = Wake::new().map_err(|e| {
        let mut err = Error::new(format!("could not create a pipeline stage's wake: {e}"), 1);
        err.span = spec.span;
        Break::Error(err)
    })?;
    let StageRoute { stdin, stdout, .. } = route;
    let stdin = super::launch::stage_stdin(stdin, cx.group, cx.shell, &wake)?;

    let group = cx.group.leader_pgid();

    let stdout = match stdout {
        ByteOut::Downstream(w) => Sink::Pipe(Arc::new(w), Arc::clone(&wake)),
        ByteOut::Parent => cx.shell.io.stdout.clone(),
    };
    let stderr = cx.shell.io.stderr.clone();

    let park = StagePark {
        gate: Arc::clone(gate),
        stop: StageStop::new(),
    };
    let policy = cx.shell.local.audit.active_policy();
    let mooring = Mooring::for_stage_thread(cx.mooring, park.clone());

    let io = Io {
        stdin,
        stdout: stdout.clone(),
        ambient: stdout,
        stderr,
        interactive: cx.shell.io.interactive,
        terminal: cx.shell.io.terminal,
        launch_role: crate::io::LaunchRole::PipelineStage(group),
    };

    let env = cx.env.clone();
    let comp = Arc::clone(stage);
    let stop = Arc::clone(&park.stop);
    let span = spec.span;

    #[cfg(unix)]
    let watcher = {
        let stop = Arc::clone(&stop);
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("ral pipeline stage interior-stop watcher".to_string())
            .spawn(move || watch_interior_stop(&stop, &tx, ix))
            .ok()
    };

    let closure_stop = Arc::clone(&stop);
    let settle = super::collect::SettleOnDrop::new(ix, tx);
    let spawned = cx.shell.spawn_thread(
        mooring,
        "ral pipeline stage",
        Arc::new(env.clone()),
        move |mooring, child| {
            child.io = io;
            child.local.audit.install_active_policy(policy);
            let child = &mut *child;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                machine::evaluate(Closure { comp, env }, mooring, child)
            }));
            let obs = match result {
                Ok(Ok(v)) => super::collect::StageObservation::ok()
                    .with_value(is_last.then_some(v))
                    .with_audit(child.local.audit.take_fragment()),
                Ok(Err(br)) => super::collect::StageObservation::from_break(br)
                    .with_audit(child.local.audit.take_fragment()),
                Err(payload) => super::collect::StageObservation::failure(panic_error(&*payload, span)),
            };
            // Release the interior-stop watcher before this stage's own last
            // act: once `Settled` is filed the collector may drop the
            // receiver at any time, and the watcher must not be found still
            // blocked on an edge that will never come.
            closure_stop.close();
            settle.send(super::collect::Settlement::Thread(obs));
        },
    );
    let (join, cancel) = spawned.map_err(|e| {
        let mut err = Error::new(format!("could not start a pipeline stage thread: {e}"), 1);
        err.span = span;
        Break::Error(err)
    })?;

    Ok(ThreadStage {
        join: Some(join),
        #[cfg(unix)]
        watcher,
        stop,
        cancel,
        wake,
        ending: EndingCell::default(),
    })
}

/// This stage's own interior stop, watched off [`StageStop`]'s condvar one
/// edge at a time — `Report::Stopped`/`Report::Continued` — rather than
/// polled.  Exits once [`StageStop::close`] runs (the stage thread's own
/// last act) or the collector's receiver is gone.
#[cfg(unix)]
fn watch_interior_stop(stop: &StageStop, tx: &std::sync::mpsc::Sender<super::collect::Report>, ix: usize) {
    let mut last = None;
    loop {
        let next = stop.wait_for_change(last);
        if stop.is_closed() {
            return;
        }
        last = next;
        let report = match next {
            Some(sig) => super::collect::Report::Stopped(ix, sig),
            None => super::collect::Report::Continued(ix),
        };
        if tx.send(report).is_err() {
            return;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use super::super::group::PipelineGroup;
    use super::super::resolve::{StageLaunch, TerminalPlan};
    use super::super::route::ByteIn;
    use crate::types::{Shell, TerminalAccess};
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn compile_one(source: &str) -> Arc<Comp> {
        let ast = crate::parse(source).expect("parse");
        let top =
            crate::elaborate(&ast, std::collections::HashSet::default(), "").expect("elaborate");
        let [phrase] = top.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", top.phrases);
        };
        let crate::ir::Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        comp.clone()
    }

    fn spec_for(comp: &Comp, feeds_pipe: bool) -> StageSpec {
        StageSpec {
            launch: StageLaunch::Thread,
            span: comp.span,
            feeds_pipe,
        }
    }

    fn prepared_group() -> PipelineGroup {
        PipelineGroup::prepare(TerminalPlan::NoTerminal, &Shell::default()).expect("anchor spawns")
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        let env = shell.env.clone();
        let mut group = prepared_group();
        let stage = compile_one("echo hi");
        let spec = spec_for(&stage, true);
        let (mut reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer),
            held: None,
        };
        let mooring = Mooring::adrift();
        let gate = StageGate::new();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle =
            launch_thread_stage(&stage, &spec, route, cx, &gate, 0, true, tx).expect("launch");

        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read stage stdout");
        assert_eq!(out, b"hi\n");

        let super::super::collect::Report::Settled(ix, super::super::collect::Settlement::Thread(obs)) =
            rx.recv().expect("the stage sends its own Settled")
        else {
            panic!("a thread stage must settle as Settlement::Thread");
        };
        assert_eq!(ix, 0);
        assert!(obs.break_.is_none(), "echo hi must not fail");
    }

    #[test]
    fn a_cancelled_spinning_stage_ends_within_500ms() {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        let env = shell.env.clone();
        let mut group = prepared_group();
        // A self-recursive, argument-incrementing call with no base case:
        // nothing but a cancel ends it.
        let stage = compile_one("!{ let go = { |n| go $[$n + 1] }; go 0 }");
        let spec = spec_for(&stage, true);
        let (_reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer),
            held: None,
        };
        let mooring = Mooring::adrift();
        let gate = StageGate::new();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut handle =
            launch_thread_stage(&stage, &spec, route, cx, &gate, 0, true, tx).expect("launch");

        handle.cancel(CancelCause::ReaderGone);
        handle.interrupt();

        let start = Instant::now();
        let super::super::collect::Report::Settled(_, super::super::collect::Settlement::Thread(obs)) = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a cancelled stage sends its own Settled within 500ms")
        else {
            panic!("a thread stage must settle as Settlement::Thread");
        };
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(obs.break_.is_some(), "a killed stage must not settle Ok");
    }

    /// `panic_error` converts a caught payload to an error carrying the
    /// stage's own span, its stack having none of its own.
    #[test]
    fn panic_error_carries_the_payload_and_span() {
        let file = Shell::default().install_script_context("<test>", "boom");
        let span = Span::new(file, 0, 4);
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");

        let err = panic_error(&*payload, Some(span));
        assert!(
            err.message.contains("boom"),
            "the panic payload must ride in the message: {}",
            err.message
        );
        assert_eq!(err.span, Some(span), "the stage's own span must survive");
    }

    #[test]
    fn a_stages_terminal_lease_is_none() {
        let mut parent = Shell::default();
        parent.session.terminal_lease = crate::process::TerminalLease::mint_at_startup(true);
        let outer = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            parent.terminal_lease(&outer).is_some(),
            "precondition: the outer mooring holds a lease"
        );

        let park = StagePark {
            gate: StageGate::new(),
            stop: StageStop::new(),
        };
        let stage_mooring = Mooring::for_stage_thread(&outer, park);
        assert!(
            parent.terminal_lease(&stage_mooring).is_none(),
            "a stage thread's mooring denies terminal access outright"
        );
    }
}
