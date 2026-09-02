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
use crate::process::{CancelCause, CancelScope, Signal, StageGate, StagePark, StageStop, Wake};
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<StageStop>,
    cancel: CancelScope,
    wake: Arc<Wake>,
    span: Option<Span>,
}

impl ThreadStage {
    /// This stage's own interior stop — a child it spawned itself stopping —
    /// read from `StagePark`'s cell, independent of whether the thread has
    /// finished: readiness arrives only by [`super::collect::Event::Settled`].
    pub(super) fn stopped(&self) -> Option<Signal> {
        self.stop.get()
    }

    pub(super) fn cancel(&self, cause: CancelCause) {
        self.cancel.cancel(cause);
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

    pub(super) fn resume(&self) {
        self.stop.set(None);
    }

    /// This stage's `Settled` event has already arrived by channel — the
    /// thread is already returning, so this reclaims it rather than waiting
    /// for it.
    pub(super) fn join_after_settled(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// This stage's sender was dropped by an unwind rather than a send: a
    /// disconnect alone carries no message, so only `join` can recover it.
    /// Surfaces as an `Error` carrying this stage's span, since the stage's
    /// own stack carries none of its own to attribute it to.
    pub(super) fn recover_panic(mut self) -> super::collect::StageObservation {
        let join = self
            .join
            .take()
            .expect("a disconnected sender's thread has not been joined yet");
        let payload = join
            .join()
            .expect_err("a disconnected sender implies its thread unwound rather than returned");
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let mut err = Error::new(format!("ral pipeline stage panicked: {msg}"), 1);
        err.span = self.span;
        super::collect::StageObservation::failure(err)
    }
}

impl Drop for ThreadStage {
    /// A stage never observed — an aborted launch, a panic elsewhere in the
    /// pipeline unwinding past it — is cancelled, interrupted, and joined
    /// rather than abandoned.
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel(CancelCause::Terminate);
        self.interrupt();
        if let Some(join) = self.join.take() {
            let _ = join.join();
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
    tx: std::sync::mpsc::Sender<super::collect::Event>,
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

    let spawned = cx.shell.spawn_thread(
        mooring,
        "ral pipeline stage",
        Arc::new(env.clone()),
        move |mooring, child| {
            child.io = io;
            child.local.audit.install_active_policy(policy);
            let result = machine::evaluate(Closure { comp, env }, mooring, child);
            let audit = child.local.audit.take_fragment();
            let obs = match result {
                Ok(v) => super::collect::StageObservation::ok()
                    .with_value(is_last.then_some(v))
                    .with_audit(audit),
                Err(br) => super::collect::StageObservation::from_break(br).with_audit(audit),
            };
            // The stage's last act, and `tx`'s only other exit is this
            // closure unwinding: a sender dropped with no `Settled` sent is
            // the disconnect the collector reads as this stage's panic.
            let _ = tx.send(super::collect::Event::Settled(ix, obs));
        },
    );
    let (join, cancel) = spawned.map_err(|e| {
        let mut err = Error::new(format!("could not start a pipeline stage thread: {e}"), 1);
        err.span = span;
        Break::Error(err)
    })?;

    Ok(ThreadStage {
        join: Some(join),
        stop,
        cancel,
        wake,
        span,
    })
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

        let super::super::collect::Event::Settled(ix, obs) =
            rx.recv().expect("the stage sends its own Settled");
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
        let handle =
            launch_thread_stage(&stage, &spec, route, cx, &gate, 0, true, tx).expect("launch");

        handle.cancel(CancelCause::ReaderGone);
        handle.interrupt();

        let start = Instant::now();
        let super::super::collect::Event::Settled(_, obs) = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a cancelled stage sends its own Settled within 500ms");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(obs.break_.is_some(), "a killed stage must not settle Ok");
    }

    /// A `ThreadStage` over a body that panics at once, with no sender: its
    /// news arrives by direct `join`, as `recover_panic` does once the
    /// collector reads its absence as a disconnect.
    fn panicking_stage(span: Span) -> ThreadStage {
        let join = std::thread::Builder::new()
            .spawn(move || panic!("boom"))
            .expect("spawn");
        ThreadStage {
            join: Some(join),
            stop: StageStop::new(),
            cancel: CancelScope::root(),
            wake: Wake::new().expect("wake"),
            span: Some(span),
        }
    }

    /// `recover_panic` joins the panicking thread directly — no probe, no
    /// polling — and converts the payload to an error carrying the stage's
    /// own span, its stack having none of its own.
    #[test]
    fn observe_of_a_panicking_body_carries_the_panic_and_its_span() {
        let file = Shell::default().install_script_context("<test>", "boom");
        let span = Span::new(file, 0, 4);
        let handle = panicking_stage(span);

        let obs = handle.recover_panic();
        match obs.break_ {
            Some(Break::Error(err)) => {
                assert!(
                    err.message.contains("boom"),
                    "the panic payload must ride in the message: {}",
                    err.message
                );
                assert_eq!(err.span, Some(span), "the stage's own span must survive");
            }
            other => panic!("expected an Error break, got {other:?}"),
        }
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
