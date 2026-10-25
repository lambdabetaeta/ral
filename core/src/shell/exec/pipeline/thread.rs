//! A ral-written pipeline stage evaluated on its own OS thread, sharing the
//! pipeline's process group with whatever externals it spawns.
//! [`ThreadStage`] is the collector's handle onto the running thread.

use super::collect::{SettleOnDrop, Slot, StageObservation};
use super::launch::LaunchCx;
use super::resolve::StageSpec;
use super::route::{ByteIn, ByteOut};
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::process::{CancelCause, POLL_SLICE, ScopeId, ScopeNode};
#[cfg(test)]
use crate::process::{DurableRoot, Scope};
use crate::shell::runtime::{Break, Error, Mooring, Settled, ThreadEnd};
use crate::source::Span;
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    cancel: ScopeNode,
}

impl ThreadStage {
    /// The collector's write on the stage's node; on Windows, one interrupt
    /// of the I/O in flight.
    pub(super) fn cancel(&self, cause: CancelCause) {
        self.cancel.cancel(cause);
        #[cfg(windows)]
        self.interrupt();
    }

    pub(super) fn id(&self) -> ScopeId {
        self.cancel.id()
    }

    #[cfg(windows)]
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// `CancelSynchronousIo` cancels only the I/O pending when it lands, so a
    /// request issued after it, or retried under a shield, needs the next.
    #[cfg(windows)]
    pub(super) fn interrupt(&self) {
        if let Some(join) = &self.join {
            crate::io::interrupt(join);
        }
    }

    /// Reclaims a thread already returning: its `Returned` has arrived.
    pub(super) fn join_after_settled(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// A `ThreadStage` around no real thread, for `collect.rs`'s own
    /// transition-table tests, and the scope it was built over.
    #[cfg(test)]
    pub(super) fn fake() -> (Self, Scope) {
        let node = DurableRoot::default().worker();
        let view = Scope::clone(&node);
        (
            Self {
                join: None,
                cancel: node,
            },
            view,
        )
    }
}

impl Drop for ThreadStage {
    /// A stage never observed is cancelled and joined, not abandoned: a slice
    /// at a time, sharing the strike `CollectState::drop` already made.
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel(CancelCause::Terminated);
        while self.join.as_ref().is_some_and(|j| !j.is_finished()) {
            #[cfg(windows)]
            self.interrupt();
            std::thread::sleep(POLL_SLICE);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A parent-side error carrying the stage's own span.
fn stage_error(message: String, span: Option<Span>) -> Break {
    Break::Error(Error {
        span,
        ..Error::new(message)
    })
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
    let node = cx.shell.scope.child();
    let stdin = super::launch::stage_stdin(stdin, cx.shell, &node)?;
    let mooring = Mooring::for_thread(cx.mooring, node.clone(), cx.mooring.surface.clone());

    let stdout = match stdout {
        ByteOut::Downstream(w, edge) => Sink::Pipe {
            writer: Arc::new(w),
            edge,
            stage: node.id(),
        },
        ByteOut::Parent => cx.shell.io.stdout.clone(),
    };

    let io = Io {
        stdin,
        stdout,
        stderr: cx.shell.io.stderr.clone(),
        interactive: cx.shell.io.interactive,
        terminal: cx.shell.io.terminal,
        stage: Some(cx.group.membership(&node)),
    };

    let policy = cx.shell.local.audit.active_policy();
    let env = cx.env.clone();
    let eval = cx.eval.run;
    let comp = Arc::clone(stage);
    let span = spec.span;

    let join = cx
        .shell
        .spawn_thread(mooring, "ral pipeline stage", move |mooring, child| {
            // Armed by the thread it guards: a spawn that fails posts nothing.
            let settle = SettleOnDrop::new(slot);
            child.io = io;
            child.local.audit.install_active_policy(policy);
            let end = ThreadEnd::of(|| eval(comp, env, mooring, child));
            settle.send(StageObservation {
                end,
                audit: child.local.audit.take_fragment(),
            });
        })
        .map_err(|e| {
            stage_error(
                format!("could not start a pipeline stage thread: {e}"),
                span,
            )
        })?;

    Ok(ThreadStage {
        join: Some(join),
        cancel: node,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::group::PipelineGroup;
    use super::super::resolve::StageLaunch;
    use super::*;
    use crate::io::Edge;
    use crate::process::DurableRoot;
    use crate::shell::runtime::{Escape, TerminalAccess};
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn compile_one(source: &str) -> Arc<Comp> {
        let ast = crate::syntax::parser::parse(source).expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], None).expect("elaborate");
        let [phrase] = top.as_slice() else {
            panic!("expected one phrase, got {top:?}");
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
        PipelineGroup::prepare(
            &crate::test_helper::core_shell(),
            std::sync::mpsc::channel().0,
        )
        .expect("anchor spawns")
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = crate::test_helper::core_shell();
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
            eval: crate::shell::evaluator::STAGE_EVAL,
            group: &group,
            holds_terminal: false,
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
        assert!(
            matches!(obs.end, ThreadEnd::Settled(Ok(_))),
            "echo hi must not fail"
        );
    }

    #[test]
    fn a_cancelled_spinning_stage_ends_within_500ms() {
        let mut shell = crate::test_helper::core_shell();
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
            eval: crate::shell::evaluator::STAGE_EVAL,
            group: &group,
            holds_terminal: false,
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
            matches!(
                &obs.end,
                ThreadEnd::Settled(Err(Break::Escape(Escape::Stopped {
                    cause: CancelCause::ReaderGone,
                    ..
                })))
            ),
            "a cancelled thread ends in the reader-gone stop: {:?}",
            obs.end
        );
    }

    #[test]
    fn a_stages_terminal_lease_is_none() {
        let mut parent = crate::test_helper::core_shell();
        parent.session.terminal_lease = crate::process::TerminalLease::mint_at_startup(true);
        let outer = Mooring {
            terminal_access: TerminalAccess::Leased,
            home: DurableRoot::default().worker(),
            cleanup_grace: Duration::ZERO,
            ..Mooring::adrift()
        };
        assert!(
            parent.terminal_lease(&outer).is_some(),
            "precondition: the outer mooring holds a lease"
        );

        let stage_mooring = Mooring::for_thread(
            &outer,
            DurableRoot::default().worker(),
            outer.surface.clone(),
        );
        assert!(
            parent.terminal_lease(&stage_mooring).is_none(),
            "a stage thread's mooring denies terminal access outright"
        );
    }
}
