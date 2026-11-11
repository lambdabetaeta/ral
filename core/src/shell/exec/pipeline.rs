//! Pipeline execution engine.  `resolve` freezes each stage's launch
//! decision, `launch` places every stage in one process group, `join` folds
//! their observations into one value.  Its collector is every external's one
//! waiter: [`Started::collect`](super::command::Started::collect) waits on a
//! lone command's child as on the one stage of the group it landed in.

pub(crate) mod anchor;
mod collect;
mod group;
mod launch;
pub(crate) mod resolve;
mod route;
mod sentinel;
mod stage;
mod thread;

use crate::ir::{Argv, Comp};
use crate::process::{GroupOwner, PreparedTerminalLoan, TerminalHold, TerminalInput};
use crate::shell::runtime::{Env, Error, Mooring, Settled, Shell, Signature, Value};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use super::command::StartFailure;
use collect::{CollectState, Event, Slot};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use collect::{collect_spawned, lone_end};
use group::PipelineGroup;
use launch::{LaunchCx, spawn_stage};
use resolve::{StageLaunch, resolve_pipeline};
use route::open_stage_routes;

/// What a stage needs of the machine, handed in by the evaluator so the
/// runtime never names it: `run` evaluates a ral stage under the node's
/// environment, `close_argv` closes a direct external's argv.
#[derive(Clone, Copy)]
pub(crate) struct StageEval {
    pub(crate) run: fn(Arc<Comp>, Env, &Mooring, &mut Shell) -> Settled<Value>,
    pub(crate) close_argv: fn(&Argv, &Env, &Signature) -> Result<Vec<Value>, Error>,
}

/// A multi-stage pipeline from its launch to its join, its stages running in
/// their own process group.
///
/// Field order is teardown order: every kill the collector makes, an unwind's
/// included, addresses a pgid its group's anchor still pins — a reaped
/// leader's pid may be reused once its group is empty.
pub(crate) struct PipeNode {
    collect: CollectState,
    group: PipelineGroup,
}

impl PipeNode {
    /// Resolve the plan, spawn every stage into one process group, and fold
    /// their observations into one value.  A launch that fails part-way is
    /// rolled back and joined all the same, and its failure is the verdict.
    /// No stage runs in the parent, so none is in tail position.
    ///
    /// `env` is the pipeline node's own lexical environment — the machine's
    /// `E` in focus, not necessarily `shell.env` — and is what each stage
    /// thread evaluates its stage under.
    pub(crate) fn run(
        stages: &[Arc<Comp>],
        env: &Env,
        eval: StageEval,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Value> {
        shell.poll()?;
        let launches = resolve_pipeline(stages, env, eval, shell)?;

        // Window start for the sandbox-denial reader.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();

        let (group, terminal, input) = match shell.group_owner(mooring) {
            GroupOwner::Stage(membership) => (
                PipelineGroup::joining(membership),
                TerminalHold::default(),
                TerminalInput::Readable,
            ),
            GroupOwner::Session { permit, .. } => {
                // Capture before even the anchor can run.
                let prepared = permit.map(PreparedTerminalLoan::capture);
                PipelineGroup::prepare(tx.clone(), prepared)?
            }
        };

        let collect = CollectState::new(
            rx,
            group.group(),
            terminal,
            shell.scope.clone(),
            mooring.cleanup_grace,
            started,
        );
        let mut node = Self { collect, group };
        let mut cx = LaunchCx {
            mooring,
            shell,
            env,
            eval,
            group: &node.group,
            input,
        };
        let launched = launch(stages, launches, &mut cx, &mut node.collect, &tx);
        // The collector keeps no sender: `recv` sees the last producer go.
        drop(tx);
        node.finish(launched, mooring, shell)
    }

    /// Roll back before settling a failed start's output, including when no
    /// stage registered. A joining node leaves group teardown to its owner.
    fn finish(
        mut self,
        launched: Result<(), StartFailure>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Value> {
        if launched.is_err() {
            self.collect.roll_back();
        }
        let value = self.join(mooring, shell);
        // A failed joined launch may leave descendants holding its streams.
        // The owner must finish rollback before we wait for those drains.
        launched
            .map_err(|failure| failure.settle(&shell.scope))
            .and(value)
    }

    /// Wait on every stage and fold their observations into one value.  The
    /// anchor ends before the fold, so every key it heard is on the channel
    /// and struck before the terminal returns; its zombie pins the pgid until
    /// the group drops, after the fold's last kill.
    fn join(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let Self {
            mut collect,
            mut group,
        } = self;
        collect.drive();
        group.end_anchor();
        let value = collect.fold(mooring, shell);
        drop(group);
        value
    }
}

/// Spawn each stage in turn onto `collect`, stopping at the first that fails.
fn launch(
    stages: &[Arc<Comp>],
    launches: Vec<StageLaunch>,
    cx: &mut LaunchCx<'_>,
    collect: &mut CollectState,
    tx: &Sender<Event>,
) -> Result<(), StartFailure> {
    let routes = open_stage_routes(stages.len())?;
    for (ix, ((stage, launch), route)) in stages.iter().zip(launches).zip(routes).enumerate() {
        cx.shell.poll()?;
        let handle = spawn_stage(stage, launch, route, cx, Slot { ix, tx: tx.clone() })?;
        collect.push(handle);
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use crate::process::{CancelCause, ScopeNode};
    use crate::test_helper::eventually;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Exercise the real failed-start boundary with no registered stage. A
    /// non-enveloped confined child forks a descendant holding stderr while
    /// `Testimony::first` waits. Only the owning pipeline may kill that group.
    #[test]
    fn a_joined_failed_start_rolls_back_before_settling_its_drains() {
        crate::test_helper::subprocess::isolated(
            "shell::exec::pipeline::tests::a_joined_failed_start_rolls_back_before_settling_its_drains",
            || {
                use super::{CollectState, PipeNode, PipelineGroup};
                use crate::io::child::Connection;
                use crate::io::{Sink, Source};
                use crate::process::{DurableRoot, Placement, Scope, TerminalHold};
                use crate::shell::runtime::{Break, Mooring};
                use crate::test_helper::terminal::{WAIT, checkpoints};
                use std::io::Read as _;
                use std::os::fd::OwnedFd;
                use std::sync::{Arc, Mutex, mpsc};
                use std::time::Instant;

                const MARKER: &[u8] = b"descendant retains stderr\n";
                for cancelled in [false, true] {
                    let home = DurableRoot::default().child();
                    let scope = Scope::clone(&home);
                    let (mut ready, output) =
                        crate::process::cloexec_socketpair().expect("readiness socket");
                    ready
                        .set_read_timeout(Some(WAIT))
                        .expect("bound descendant readiness and EOF");
                    let stdout = Sink::File(Arc::new(std::fs::File::from(OwnedFd::from(output))));
                    let (report, writer) =
                        crate::process::cloexec_socketpair().expect("startup report");
                    let witness = crate::process::report::Witness::new(
                        writer.try_clone().expect("witness copy").into(),
                    )
                    .expect("report socket");
                    let report = crate::process::report::Report::new(report, writer.into());
                    let (mut observer, blocker) = checkpoints().expect("sink checkpoints");
                    let blocker = Mutex::new(blocker);
                    let (capture, bytes) = crate::io::new_buffer();
                    let stderr = Sink::Tee(
                        Box::new(Sink::Watch {
                            line: Arc::new(move |line| {
                                assert_eq!(line, &MARKER[..MARKER.len() - 1]);
                                let mut blocker = blocker.lock().unwrap();
                                blocker.send(b'B').expect("sink entered");
                                blocker.expect(b'R').expect("release sink");
                            }),
                            pending: Vec::new(),
                        }),
                        Box::new(capture),
                    );
                    let (failed_tx, failed_rx) = mpsc::channel();
                    let (done_tx, done_rx) = mpsc::channel();
                    let worker = std::thread::spawn(move || {
                        let mut shell = crate::test_helper::core_shell();
                        shell.scope = scope;
                        let mooring = Mooring::adrift();
                        let (tx, rx) = mpsc::channel();
                        let (group, _, _) =
                            PipelineGroup::prepare(tx.clone(), None).expect("owner anchor");
                        let collect = CollectState::new(
                            rx,
                            group.group(),
                            TerminalHold::default(),
                            shell.scope.clone(),
                            mooring.cleanup_grace,
                            Instant::now(),
                        );
                        let command = crate::shell::exec::command::tests::joined_startup_report(
                            &mut shell,
                            // Readiness follows the descendant's stderr write.
                            // sleep bounds its life even if the regression fails.
                            "(printf 'descendant retains stderr\n' >&2; printf D; exec /bin/sleep 20) & wait",
                            report,
                        );
                        let io = Connection::new(&Source::Empty, &stdout, &stderr)
                            .expect("captured stderr");
                        drop(stdout);
                        drop(stderr);
                        let Err(failure) = command.start(
                            io,
                            Placement::Join(group.membership(&shell.scope)),
                            &shell.scope,
                        ) else {
                            panic!("the startup report must refuse or cancel");
                        };
                        assert_eq!(matches!(*failure.reason, Break::Escape(_)), cancelled);
                        failed_tx
                            .send(())
                            .expect("start returned ownership of drains");
                        drop(tx);
                        let node = PipeNode { collect, group };
                        let settled = node.finish(Err(failure), &mooring, &mut shell);
                        assert_eq!(matches!(settled, Err(Break::Escape(_))), cancelled);
                        if let Err(Break::Error(error)) = &settled {
                            assert!(matches!(&error.reason, crate::fact::Reason::Launch(failure)
                                if matches!(**failure, crate::fact::LaunchFailure::Warrant(crate::fact::WarrantFailure::Magic))));
                        }
                        done_tx.send(settled.is_err()).expect("settled failure");
                    });
                    let mut phase = [0];
                    ready
                        .read_exact(&mut phase)
                        .expect("descendant wrote stderr and is live");
                    assert_eq!(phase, [b'D']);
                    observer.expect(b'B').expect("drain is blocked in the sink");
                    if cancelled {
                        home.cancel(CancelCause::Aborted);
                        // Keep the report open: cancellation, not EOF, ends first().
                    } else {
                        witness.refused(&crate::fact::LaunchFailure::Warrant(
                            crate::fact::WarrantFailure::Magic,
                        ));
                    }
                    failed_rx
                        .recv_timeout(WAIT)
                        .expect("failed start must return before joining drains");
                    assert_eq!(
                        ready
                            .read(&mut phase)
                            .expect("owner rollback closes descendant output"),
                        0
                    );
                    let premature = done_rx.recv_timeout(Duration::from_millis(200));
                    observer.send(b'R').expect("release sink after rollback");
                    assert!(
                        matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)),
                        "failure escaped before drain settlement: {premature:?}"
                    );
                    assert!(
                        done_rx
                            .recv_timeout(WAIT)
                            .expect("settlement completes after release")
                    );
                    worker.join().expect("pipeline settlement worker");
                    let retained = bytes.take();
                    assert_eq!(retained.bytes, MARKER);
                    assert_eq!(retained.total, MARKER.len() as u64);
                    assert!(retained.is_whole());
                }
            },
        );
    }

    /// A `sh` that appends a line to `hits` for every SIGINT or SIGTERM it
    /// hears and carries on, writing its pid to `ready` once the trap is set.
    struct Trapper(tempfile::TempDir);

    impl Trapper {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("a temp dir"))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.path().join(name)
        }

        /// The trapper's own `sh` script.
        fn script(&self) -> String {
            format!(
                "trap \"echo x >> {}\" INT TERM; echo $$ > {}; while :; do sleep 0.05; done",
                self.path("hits").display(),
                self.path("ready").display(),
            )
        }

        fn command(&self) -> String {
            format!("sh -c '{}'", self.script())
        }

        fn pid(&self) -> i32 {
            eventually(Duration::from_secs(5), || {
                let read = std::fs::read_to_string(self.path("ready")).unwrap_or_default();
                read.strip_suffix('\n').and_then(|p| p.parse().ok())
            })
            .expect("the trapper never set its trap")
        }

        fn hits(&self) -> usize {
            std::fs::read_to_string(self.path("hits"))
                .unwrap_or_default()
                .lines()
                .count()
        }
    }

    /// Run `src`, calling `strike` with the run's scope and the trapper's pid
    /// once its trap is set; the run's own end is not what these tests read.
    fn run_struck(src: &str, trapper: &Trapper, strike: impl FnOnce(&ScopeNode, i32) + Send) {
        let mut shell = crate::test_helper::core_shell();
        let scope = shell.run_cancel_handle();
        std::thread::scope(|s| {
            s.spawn(|| strike(&scope, trapper.pid()));
            let _ = shell.run_under(
                &scope,
                crate::shell::run::RunRequest::from(crate::protocol::Run::captured(
                    src,
                    crate::source::SourceOrigin::named("<test>"),
                )),
            );
        });
    }

    /// The owner's group signal is the member's one delivery: its own waiter
    /// sends no second copy.
    #[test]
    fn a_struck_frame_signals_a_stage_member_once() {
        let trapper = Trapper::new();
        run_struck(
            &format!("!{{ {} }} | /bin/cat", trapper.command()),
            &trapper,
            |scope, _| {
                scope.cancel(CancelCause::Interrupted);
            },
        );
        assert_eq!(trapper.hits(), 1);
    }

    /// The same when the kernel delivered to the group, as the tty does: with
    /// no terminal lent the report is inert, so nothing sends a second copy,
    /// and only a root abort, which has no grace, ends the trapper.
    #[test]
    fn a_group_signal_reaches_a_stage_member_once() {
        let trapper = Trapper::new();
        run_struck(
            &format!("!{{ {} }} | /bin/cat", trapper.command()),
            &trapper,
            |scope, pid| {
                let pid = rustix::process::Pid::from_raw(pid).expect("a live pid");
                let pgid = rustix::process::getpgid(Some(pid)).expect("the trapper's group");
                unsafe { libc::kill(-pgid.as_raw_nonzero().get(), libc::SIGINT) };
                eventually(Duration::from_secs(5), || {
                    (trapper.hits() > 0).then_some(())
                })
                .expect("the trapper never heard SIGINT");
                // Room for a second copy, were anything to send one.
                std::thread::sleep(Duration::from_millis(300));
                scope.cancel(CancelCause::Aborted);
            },
        );
        assert_eq!(trapper.hits(), 1);
    }

    /// A nested pipeline's collector answers to the outer stage's scope, so a
    /// member of it hears the outermost owner's one SIGTERM.
    #[test]
    fn a_nested_pipelines_member_hears_the_outer_owner_once() {
        let trapper = Trapper::new();
        run_struck(
            &format!("!{{ {} | /bin/cat }} | /bin/cat", trapper.command()),
            &trapper,
            |scope, _| scope.cancel(CancelCause::TimedOut),
        );
        assert_eq!(trapper.hits(), 1);
    }

    /// The audit's P4: Ctrl-C lands while the pipeline launches, then a later
    /// stage fails to.  The rollback and join strike the key on the run's node
    /// and tear the started stage down.  The stage traps the key and the grace
    /// signal, so the kill waits out the grace and cannot race the anchor's
    /// report of the key.
    #[test]
    fn a_key_heard_while_a_launch_failed_is_struck() {
        crate::test_helper::subprocess::isolated(
            "shell::exec::pipeline::tests::a_key_heard_while_a_launch_failed_is_struck",
            || {
                use super::stage::StageHandle;
                use super::{CollectState, PipeNode, PipelineGroup, Slot};
                use crate::shell::runtime::Mooring;

                let trapper = Trapper::new();
                let mut shell = crate::test_helper::core_shell();
                let mooring = Mooring::adrift();
                let frame = crate::process::DurableRoot::default().child();
                let (tx, rx) = std::sync::mpsc::channel();
                let (group, _, _) =
                    PipelineGroup::prepare(tx.clone(), None).expect("anchor spawns");
                let mut cmd = crate::process::Launch::new("/bin/sh");
                cmd.args(["-c", &trapper.script()]);
                let child = cmd
                    .spawn(
                        crate::process::Placement::Join(group.membership(&frame)),
                        &frame,
                    )
                    .expect("spawn the trapper into the group")
                    .child;
                let mut collect = CollectState::new(
                    rx,
                    group.group(),
                    {
                        let lease =
                            crate::process::TerminalLease::mint_at_startup(true).expect("lease");
                        let permit = crate::process::TerminalAccess::Leased
                            .reach(Some(&lease), &frame, true)
                            .expect("permit");
                        crate::process::PreparedTerminalLoan::capture(permit).assumed()
                    },
                    shell.scope.clone(),
                    mooring.cleanup_grace,
                    std::time::Instant::now(),
                );
                collect.push(StageHandle::for_test(Slot { ix: 0, tx }, child));
                let pid = trapper.pid();
                // As the tty delivers a key: to the whole group, the anchor included.
                group
                    .group()
                    .owned()
                    .expect("owning group")
                    .signal_group(crate::process::Signal::new(libc::SIGINT));

                let mut node = PipeNode { collect, group };
                node.collect.roll_back();
                let _ = node.join(&mooring, &mut shell);
                assert_eq!(
                    frame.cause(),
                    Some(CancelCause::Interrupted),
                    "a key heard during the launch must be struck on the run's node"
                );
                assert_ne!(
                    unsafe { libc::kill(pid, 0) },
                    0,
                    "the started stage must be torn down"
                );
            },
        );
    }
}
