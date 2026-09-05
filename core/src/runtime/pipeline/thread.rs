//! A stage thread: a ral-written pipeline stage evaluated on its own OS
//! thread, sharing the pipeline's process group with whatever externals it
//! spawns.  [`launch_thread_stage`] is the parent side — it wires the
//! stage's `Io` from its [`StageRoute`] and hands the closure to
//! [`Shell::spawn_thread`]; [`ThreadStage`] is the collector's handle onto
//! the running thread.

use super::collect::Slot;
use super::launch::LaunchCx;
use super::resolve::StageSpec;
use super::route::{ByteOut, StageRoute};
use crate::evaluator::machine;
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::source::Span;
use crate::types::{Break, Closure, Error, Mooring, Settled};
use crate::process::{CancelCause, CancelScope, Wake};
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    cancel: CancelScope,
    wake: Arc<Wake>,
}

impl ThreadStage {
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

    /// This stage's `Returned` event has already arrived by channel — the
    /// thread is already returning, so this reclaims it rather than waiting
    /// for it.
    pub(super) fn join_after_settled(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// A `ThreadStage` around no real thread, for `collect.rs`'s own
    /// transition-table tests: all it owes them is a no-op join.
    #[cfg(test)]
    pub(super) fn fake_for_step_test() -> Self {
        Self {
            join: None,
            cancel: CancelScope::root(),
            wake: Wake::new().expect("create a wake"),
        }
    }
}

/// Turn a caught panic's payload into the `Error` a stage's own `Returned`
/// carries, stamped with the stage's span — the panicking thread's stack has
/// none of its own to attribute it to.
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
pub(super) fn launch_thread_stage(
    stage: &Arc<Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: &LaunchCx<'_>,
    slot: Slot,
    is_last: bool,
) -> Settled<ThreadStage> {
    let wake = Wake::new().map_err(|e| {
        let mut err = Error::new(format!("could not create a pipeline stage's wake: {e}"), 1);
        err.span = spec.span;
        Break::Error(err)
    })?;
    let StageRoute { stdin, stdout, .. } = route;
    let stdin = super::launch::stage_stdin(stdin, cx.shell, &wake)?;

    let group = cx.group.leader_pgid();

    let stdout = match stdout {
        ByteOut::Downstream(w, edge) => Sink::Pipe {
            writer: Arc::new(w),
            wake: Arc::clone(&wake),
            edge,
        },
        ByteOut::Parent => cx.shell.io.stdout.clone(),
    };
    let stderr = cx.shell.io.stderr.clone();

    let policy = cx.shell.local.audit.active_policy();
    let mooring = Mooring::for_stage_thread(cx.mooring);

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
    let span = spec.span;

    let settle = super::collect::SettleOnDrop::new(slot);
    let spawned = cx.shell.spawn_thread(
        mooring,
        "ral pipeline stage",
        Arc::new(env.clone()),
        move |mooring, child| {
            child.io = io;
            child.local.audit.install_active_policy(policy);
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
            settle.send(obs);
        },
    );
    let (join, cancel) = spawned.map_err(|e| {
        let mut err = Error::new(format!("could not start a pipeline stage thread: {e}"), 1);
        err.span = span;
        Break::Error(err)
    })?;

    Ok(ThreadStage {
        join: Some(join),
        cancel,
        wake,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use super::super::group::PipelineGroup;
    use super::super::resolve::StageLaunch;
    use super::super::route::ByteIn;
    use crate::io::Edge;
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

    fn spec_for(comp: &Comp) -> StageSpec {
        StageSpec {
            launch: StageLaunch::Thread,
            span: comp.span,
        }
    }

    /// An owning group whose channel nobody reads: these tests watch a single
    /// stage's own reports, never the anchor's.
    fn prepared_group() -> PipelineGroup {
        PipelineGroup::prepare(&Shell::default(), std::sync::mpsc::channel().0)
            .expect("anchor spawns")
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        let env = shell.env.clone();
        let mut group = prepared_group();
        let stage = compile_one("echo hi");
        let spec = spec_for(&stage);
        let (mut reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer, Edge::new()),
            held: None,
        };
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = launch_thread_stage(&stage, &spec, route, &cx, Slot { ix: 0, tx }, true)
            .expect("launch");

        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read stage stdout");
        assert_eq!(out, b"hi\n");

        let super::super::collect::Event::Returned(ix, obs) =
            rx.recv().expect("the stage sends its own Returned")
        else {
            panic!("a thread stage must settle as Event::Returned");
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
        let spec = spec_for(&stage);
        let (_reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer, Edge::new()),
            held: None,
        };
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = launch_thread_stage(&stage, &spec, route, &cx, Slot { ix: 0, tx }, true)
            .expect("launch");

        handle.cancel(CancelCause::ReaderGone);
        handle.interrupt();

        let start = Instant::now();
        let super::super::collect::Event::Returned(_, obs) = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a cancelled stage sends its own Returned within 500ms")
        else {
            panic!("a thread stage must settle as Event::Returned");
        };
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(
            matches!(&obs.break_, Some(Break::Error(e)) if e.cancelled_by() == Some(CancelCause::ReaderGone)),
            "a real cancelled thread's break must carry the cancel's own mark: {:?}",
            obs.break_
        );
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

        let stage_mooring = Mooring::for_stage_thread(&outer);
        assert!(
            parent.terminal_lease(&stage_mooring).is_none(),
            "a stage thread's mooring denies terminal access outright"
        );
    }
}
