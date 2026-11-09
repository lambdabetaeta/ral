//! A ral-written pipeline stage evaluated on its own OS thread, sharing the
//! pipeline's process group with whatever externals it spawns.
//! [`ThreadStage`] is the collector's handle onto the running thread.

use super::collect::{SettleOnDrop, Slot, StageObservation};
use super::launch::LaunchCx;
use super::route::{ByteIn, ByteOut, HeldEdge};
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::process::{CancelCause, POLL_SLICE, ScopeId, ScopeNode};
#[cfg(test)]
use crate::process::{DurableRoot, Scope};
use crate::shell::runtime::{Mooring, Settled};
use std::sync::Arc;

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<()>>,
    home: ScopeNode,
}

impl ThreadStage {
    /// The collector's write on the stage's node; on Windows, one interrupt
    /// of the I/O in flight.
    pub(super) fn cancel(&self, cause: CancelCause) {
        self.home.cancel(cause);
        #[cfg(windows)]
        self.interrupt();
    }

    pub(super) fn id(&self) -> ScopeId {
        self.home.id()
    }

    #[cfg(windows)]
    pub(super) fn is_cancelled(&self) -> bool {
        self.home.is_cancelled()
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
        let node = DurableRoot::default().child();
        let view = Scope::clone(&node);
        (
            Self {
                join: None,
                home: node,
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

/// Spawn one ral-written stage on its own thread, which writes an edge as a
/// `Sink::Pipe` naming itself.
pub(super) fn launch_thread_stage(
    stage: &Arc<Comp>,
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &LaunchCx<'_>,
    slot: Slot,
) -> Settled<(ThreadStage, Option<HeldEdge>)> {
    let node = cx.shell.scope.child();
    let stdin = super::launch::stage_stdin(stdin, cx.shell, &node, cx.input)?;
    let mooring = Mooring::for_thread(cx.mooring, node.clone(), cx.mooring.surface.clone());

    let (stdout, held) = match stdout {
        ByteOut::Parent => (cx.shell.io.stdout.clone(), None),
        ByteOut::Downstream { writer, held } => (
            Sink::Pipe {
                writer: Arc::new(writer),
                edge: Arc::clone(&held.edge),
                stage: node.id(),
            },
            Some(held),
        ),
    };

    let io = Io {
        stdin,
        stdout,
        stderr: cx.shell.io.stderr.clone(),
    };
    let membership = cx.group.membership(&node);

    let policy = cx.shell.local.audit.active_policy();
    let env = cx.env.clone();
    let eval = cx.eval.run;
    let comp = Arc::clone(stage);

    let join = cx
        .shell
        .spawn_stage(mooring, io, membership, move |mooring, child| {
            // Armed by the thread it guards: a spawn that fails posts nothing.
            let settle = SettleOnDrop::new(slot);
            child.local.audit.install_active_policy(policy);
            let end = child
                .base(&mooring.home, |child| eval(comp, env, mooring, child))
                .flush_streams(&mut child.io)
                .leaving(&child.session.sources);
            settle.send(StageObservation {
                end,
                audit: child.local.audit.take_fragment(),
            });
        })?;

    Ok((
        ThreadStage {
            join: Some(join),
            home: node,
        },
        held,
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::group::PipelineGroup;
    use super::*;
    use crate::io::Edge;
    use crate::process::TerminalAccess;
    use crate::shell::runtime::{End, Stop};
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

    /// An owning group whose channel nobody reads: these tests watch a single
    /// stage's own reports, never the anchor's.
    fn prepared_group() -> PipelineGroup {
        PipelineGroup::prepare(std::sync::mpsc::channel().0, None)
            .expect("anchor spawns")
            .0
    }

    fn downstream(writer: os_pipe::PipeWriter, reader: &os_pipe::PipeReader) -> ByteOut {
        ByteOut::Downstream {
            writer,
            held: HeldEdge {
                edge: Edge::new(),
                reader: Arc::new(reader.try_clone().expect("hold")),
                listening: std::cell::Cell::default(),
            },
        }
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = crate::test_helper::core_shell();
        let env = shell.env.clone();
        let group = prepared_group();
        let stage = compile_one("echo hi");
        let (mut reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            eval: crate::shell::evaluator::STAGE_EVAL,
            group: &group,
            input: crate::process::TerminalInput::Withheld,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (_handle, _held) = launch_thread_stage(
            &stage,
            ByteIn::Parent,
            downstream(writer, &reader),
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
        assert!(matches!(obs.end, End::Ok(_)), "echo hi must not fail");
    }

    #[test]
    fn a_stage_flushes_both_streams_before_returned_and_preserves_the_first_panic() {
        use crate::sync::LockExt;

        for (body_panic, stdout_panic, stderr_panic, expected_panic) in [
            (false, false, false, None),
            (true, false, false, Some("body-panic")),
            (true, true, true, Some("body-panic")),
            (false, true, true, Some("stdout-flush-panic")),
            (false, false, true, Some("stderr-flush-panic")),
        ] {
            let mut shell = crate::test_helper::core_shell();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let watch = |name: &'static str, panics: bool| {
                let seen = seen.clone();
                Sink::Watch {
                    line: Arc::new(move |line| {
                        seen.lock_ignore_poison().push((name, line.to_vec()));
                        assert!(!panics, "{name}-flush-panic");
                    }),
                    pending: Vec::new(),
                }
            };
            shell.io.stdout = watch("stdout", stdout_panic);
            shell.io.stderr = watch("stderr", stderr_panic);
            let env = shell.env.clone();
            let group = prepared_group();
            let mooring = Mooring::adrift();
            let cx = LaunchCx {
                mooring: &mooring,
                shell: &mut shell,
                env: &env,
                eval: super::super::StageEval {
                    run: if body_panic {
                        |_, _, _, child| {
                            child.write_stdout(b"out-tail").unwrap();
                            child.write_stderr(b"err-tail").unwrap();
                            panic!("body-panic");
                        }
                    } else {
                        |_, _, _, child| {
                            child.write_stdout(b"out-tail").unwrap();
                            child.write_stderr(b"err-tail").unwrap();
                            Ok(crate::shell::runtime::Value::Unit)
                        }
                    },
                    ..crate::shell::evaluator::STAGE_EVAL
                },
                group: &group,
                input: crate::process::TerminalInput::Withheld,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let (stage, _held) = launch_thread_stage(
                &compile_one("echo unused"),
                ByteIn::Parent,
                ByteOut::Parent,
                &cx,
                Slot { ix: 0, tx },
            )
            .expect("launch");

            let super::super::collect::Event::Returned(_, obs) = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("stage finalization publishes Returned even when flushing panics")
            else {
                panic!("a thread stage must settle as Event::Returned");
            };
            assert_eq!(
                *seen.lock_ignore_poison(),
                [
                    ("stdout", b"out-tail".to_vec()),
                    ("stderr", b"err-tail".to_vec())
                ],
                "both stage-local tails must be attempted before publication"
            );
            match expected_panic {
                Some(expected) => assert!(
                    matches!(&obs.end, End::Panic(text) if text == expected),
                    "expected {expected}, got {:?}",
                    obs.end
                ),
                None => assert!(matches!(obs.end, End::Ok(_))),
            }
            stage.join_after_settled();
            assert_eq!(seen.lock_ignore_poison().len(), 2, "no tail is retried");
        }
    }

    #[test]
    fn a_cancelled_spinning_stage_ends_within_500ms() {
        let mut shell = crate::test_helper::core_shell();
        let env = shell.env.clone();
        let group = prepared_group();
        // A self-recursive, argument-incrementing call with no base case:
        // nothing but a cancel ends it.
        let stage = compile_one("!{ let go = { |n| go $[$n + 1] }; go 0 }");
        let (reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            eval: crate::shell::evaluator::STAGE_EVAL,
            group: &group,
            input: crate::process::TerminalInput::Withheld,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (handle, _held) = launch_thread_stage(
            &stage,
            ByteIn::Parent,
            downstream(writer, &reader),
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
                End::Stopped(Stop {
                    cause: CancelCause::ReaderGone,
                    ..
                })
            ),
            "a cancelled thread ends in the reader-gone stop: {:?}",
            obs.end
        );
    }

    /// A stage's base reads its node once more after its body: a cut that
    /// lands after the body's last poll is the stage's ending, naming it.
    #[test]
    fn a_stage_reads_its_node_once_more_after_its_body() {
        let mut shell = crate::test_helper::core_shell();
        let env = shell.env.clone();
        let group = prepared_group();
        let (reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let mooring = Mooring::adrift();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            eval: super::super::StageEval {
                run: |_, _, mooring, _| {
                    mooring.home.cancel(CancelCause::ReaderGone);
                    Ok(crate::shell::runtime::Value::Unit)
                },
                ..crate::shell::evaluator::STAGE_EVAL
            },
            group: &group,
            input: crate::process::TerminalInput::Withheld,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (stage, _held) = launch_thread_stage(
            &compile_one("echo unused"),
            ByteIn::Parent,
            downstream(writer, &reader),
            &cx,
            Slot { ix: 0, tx },
        )
        .expect("launch");

        let super::super::collect::Event::Returned(_, obs) =
            rx.recv().expect("the stage sends its own Returned")
        else {
            panic!("a thread stage must settle as Event::Returned");
        };
        assert!(
            matches!(
                &obs.end,
                End::Stopped(Stop {
                    cause: CancelCause::ReaderGone,
                    owner: Some(owner),
                    ..
                }) if *owner == stage.id()
            ),
            "the closing poll ends the stage in its own cut: {:?}",
            obs.end
        );
    }

    /// Exercise `PipeNode`'s Stage branch twice, not just `Source::offered`:
    /// the stage mooring denies access, but cannot replace its owner's stdin.
    #[test]
    fn a_nested_pipeline_retains_its_owners_input_decision() {
        crate::test_helper::subprocess::isolated(
            "shell::exec::pipeline::thread::tests::a_nested_pipeline_retains_its_owners_input_decision",
            || {
                fn nested(
                    env: &crate::shell::runtime::Env,
                    mooring: &Mooring,
                    shell: &mut crate::shell::runtime::Shell,
                    readable: bool,
                ) -> Settled<crate::shell::runtime::Value> {
                    assert!(matches!(
                        (readable, &shell.io.stdin),
                        (true, crate::io::Source::Terminal) | (false, crate::io::Source::Empty)
                    ));
                    assert_eq!(mooring.terminal_access, TerminalAccess::Denied);
                    assert!(matches!(
                        shell.group_owner(mooring),
                        crate::process::GroupOwner::Stage(_)
                    ));
                    let eval = super::super::StageEval {
                        run: if readable {
                            |_, _, _, child| {
                                assert!(matches!(child.io.stdin, crate::io::Source::Terminal));
                                Ok(crate::shell::runtime::Value::Unit)
                            }
                        } else {
                            |_, _, _, child| {
                                assert!(matches!(child.io.stdin, crate::io::Source::Empty));
                                Ok(crate::shell::runtime::Value::Unit)
                            }
                        },
                        ..crate::shell::evaluator::STAGE_EVAL
                    };
                    super::super::PipeNode::run(
                        &[compile_one("echo unused")],
                        env,
                        eval,
                        mooring,
                        shell,
                    )
                }

                let owner = prepared_group();
                for input in [
                    crate::process::TerminalInput::Readable,
                    crate::process::TerminalInput::Withheld,
                ] {
                    let readable = matches!(input, crate::process::TerminalInput::Readable);
                    let mut shell = crate::test_helper::core_shell();
                    shell.io.stdin = crate::io::Source::Terminal;
                    let stage = shell.scope.child();
                    shell.io.stdin =
                        super::super::launch::stage_stdin(ByteIn::Parent, &shell, &stage, input)
                            .expect("owner-selected input");
                    shell.session.stage = Some(owner.membership(&stage));
                    let env = shell.env.clone();
                    let eval = super::super::StageEval {
                        run: if readable {
                            |_, env, mooring, child| nested(&env, mooring, child, true)
                        } else {
                            |_, env, mooring, child| nested(&env, mooring, child, false)
                        },
                        ..crate::shell::evaluator::STAGE_EVAL
                    };
                    super::super::PipeNode::run(
                        &[compile_one("echo unused")],
                        &env,
                        eval,
                        &Mooring::adrift(),
                        &mut shell,
                    )
                    .expect("nested pipeline keeps the owner's input");
                }
            },
        );
    }
}
