//! A ral-written pipeline stage evaluated on its own OS thread, sharing the
//! pipeline's process group with whatever externals it spawns.
//! [`ThreadStage`] is the collector's handle onto the running thread.

use super::collect::{SettleOnDrop, Slot, StageObservation};
use super::launch::LaunchCx;
use super::resolve::StageSpec;
use super::route::{ByteIn, ByteOut};
use crate::evaluator::machine;
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::process::{CancelCause, CancelScope, Wake};
use crate::source::Span;
use crate::types::{Break, Closure, Error, Mooring, Settled};
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    cancel: CancelScope,
    wake: Arc<Wake>,
}

impl ThreadStage {
    /// Cancel the scope and wake the thread.  Windows has no wake an fd poll
    /// can see, so a blocked `ReadFile` is retried with `CancelSynchronousIo`
    /// until the wake is acknowledged or the thread has finished on its own.
    pub(super) fn cancel(&self, cause: CancelCause) {
        self.cancel.cancel(cause);
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

    /// Reclaims a thread already returning: its `Returned` has arrived.
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

impl Drop for ThreadStage {
    /// A stage never observed is cancelled and joined, not abandoned.
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel(CancelCause::Terminate);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A parent-side error carrying the stage's own span.
fn stage_error(message: String, span: Option<Span>) -> Break {
    Break::Error(Error {
        span,
        ..Error::new(message, 1)
    })
}

/// Stamped with the stage's span — the panicking thread's stack has none of
/// its own to attribute it to.
fn panic_error(payload: &(dyn std::any::Any + Send), span: Option<Span>) -> Error {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    Error {
        span,
        ..Error::new(format!("ral pipeline stage panicked: {msg}"), 1)
    }
}

/// Spawn one ral-written stage on its own thread.
pub(super) fn launch_thread_stage(
    stage: &Arc<Comp>,
    spec: &StageSpec,
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &LaunchCx<'_>,
    slot: Slot,
) -> Settled<ThreadStage> {
    let wake = Wake::new().map_err(|e| {
        stage_error(
            format!("could not create a pipeline stage's wake: {e}"),
            spec.span,
        )
    })?;
    let stdin = super::launch::stage_stdin(stdin, cx.shell, &wake)?;

    let group = cx.group.leader_pgid();

    // A non-final stage's ambient is its own pipe; the final stage's is the
    // parent's, which `Io::ambient` requires never be a capture buffer.
    let (stdout, ambient) = match stdout {
        ByteOut::Downstream(w, edge) => {
            let sink = Sink::Pipe {
                writer: Arc::new(w),
                wake: Arc::clone(&wake),
                edge,
            };
            (sink.clone(), sink)
        }
        ByteOut::Parent => (cx.shell.io.stdout.clone(), cx.shell.io.ambient.clone()),
    };

    let io = Io {
        stdin,
        stdout,
        ambient,
        stderr: cx.shell.io.stderr.clone(),
        interactive: cx.shell.io.interactive,
        terminal: cx.shell.io.terminal,
        launch_role: crate::io::LaunchRole::PipelineStage(group),
    };

    let policy = cx.shell.local.audit.active_policy();
    let mooring = Mooring::for_stage_thread(cx.mooring);
    let env = cx.env.clone();
    let comp = Arc::clone(stage);
    let span = spec.span;

    let settle = SettleOnDrop::new(slot);
    let spawned = cx.shell.spawn_thread(
        mooring,
        "ral pipeline stage",
        env.clone(),
        move |mooring, child| {
            child.io = io;
            child.local.audit.install_active_policy(policy);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                machine::evaluate(Closure { comp, env }, mooring, child)
            }));
            let settled = match result {
                Ok(settled) => settled,
                Err(payload) => Err(Break::Error(panic_error(&*payload, span))),
            };
            settle.send(StageObservation {
                settled,
                audit: child.local.audit.take_fragment(),
            });
        },
    );
    let (join, cancel) = spawned.map_err(|e| {
        stage_error(
            format!("could not start a pipeline stage thread: {e}"),
            span,
        )
    })?;

    Ok(ThreadStage {
        join: Some(join),
        cancel,
        wake,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::group::PipelineGroup;
    use super::super::resolve::StageLaunch;
    use super::*;
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

    fn shell_with_builtins() -> Shell {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        shell
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = shell_with_builtins();
        let env = shell.env.clone();
        let group = prepared_group();
        let stage = compile_one("echo hi");
        let spec = spec_for(&stage);
        let (mut reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = launch_thread_stage(
            &stage,
            &spec,
            ByteIn::Parent,
            ByteOut::Downstream(writer, Edge::new()),
            &cx,
            Slot { ix: 0, tx },
        )
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
        assert!(obs.settled.is_ok(), "echo hi must not fail");
    }

    /// A final stage's discarded statement writes to the parent's ambient
    /// sink, never to the parent's stdout — which under a capture is a buffer.
    #[test]
    fn a_final_stages_discarded_statement_writes_to_the_parents_ambient() {
        let mut shell = shell_with_builtins();
        let (stdout_sink, stdout_buf) = crate::io::new_buffer();
        let (ambient_sink, ambient_buf) = crate::io::new_buffer();
        shell.io.stdout = stdout_sink;
        shell.io.ambient = ambient_sink;
        let env = shell.env.clone();
        let group = prepared_group();
        let stage = compile_one("!{ echo x; echo y }");
        let spec = spec_for(&stage);
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = launch_thread_stage(
            &stage,
            &spec,
            ByteIn::Parent,
            ByteOut::Parent,
            &cx,
            Slot { ix: 0, tx },
        )
        .expect("launch");

        let super::super::collect::Event::Returned(_, obs) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the stage sends its own Returned")
        else {
            panic!("a thread stage must settle as Event::Returned");
        };
        assert!(obs.settled.is_ok(), "the stage must not fail");
        handle.join_after_settled();

        assert_eq!(crate::io::take_buffer(&stdout_buf), b"y\n");
        assert_eq!(crate::io::take_buffer(&ambient_buf), b"x\n");
    }

    #[test]
    fn a_cancelled_spinning_stage_ends_within_500ms() {
        let mut shell = shell_with_builtins();
        let env = shell.env.clone();
        let group = prepared_group();
        // A self-recursive, argument-incrementing call with no base case:
        // nothing but a cancel ends it.
        let stage = compile_one("!{ let go = { |n| go $[$n + 1] }; go 0 }");
        let spec = spec_for(&stage);
        let (_reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &group,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = launch_thread_stage(
            &stage,
            &spec,
            ByteIn::Parent,
            ByteOut::Downstream(writer, Edge::new()),
            &cx,
            Slot { ix: 0, tx },
        )
        .expect("launch");

        handle.cancel(CancelCause::ReaderGone);

        let start = Instant::now();
        let super::super::collect::Event::Returned(_, obs) = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a cancelled stage sends its own Returned within 500ms")
        else {
            panic!("a thread stage must settle as Event::Returned");
        };
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(
            matches!(&obs.settled, Err(Break::Error(e)) if e.cancelled_by() == Some(CancelCause::ReaderGone)),
            "a real cancelled thread's break must carry the cancel's own mark: {:?}",
            obs.settled
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
