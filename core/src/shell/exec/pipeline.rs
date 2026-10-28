//! Pipeline execution engine.  `resolve` freezes the terminal handoff and each
//! stage's launch decision, `launch` places every stage in one process group,
//! `join` folds their observations into one value.

pub(crate) mod anchor;
mod collect;
mod group;
mod launch;
pub(crate) mod resolve;
mod route;
mod sentinel;
mod stage;
mod thread;

use crate::ir::{Args, Comp};
use crate::shell::runtime::{Env, Error, Mooring, Settled, Shell, Signature, Value};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use collect::{CollectState, Event, Slot};
use group::PipelineGroup;
use launch::{LaunchCx, spawn_stage};
use resolve::{StageSpec, TerminalPlan, resolve_pipeline};
use route::open_stage_routes;

/// What a stage needs of the machine, handed in by the evaluator so the
/// runtime never names it: `run` evaluates a ral stage under the node's
/// environment, `close_args` closes a direct external's arguments.
#[derive(Clone, Copy)]
pub(crate) struct StageEval {
    pub(crate) run: fn(Arc<Comp>, Env, &Mooring, &mut Shell) -> Settled<Value>,
    pub(crate) close_args: fn(&Args, &Env, &Signature) -> Result<Vec<Value>, Error>,
}

/// A multi-stage pipeline from its launch to its join, its stages running in
/// their own process group.
///
/// Field order is teardown order: a collector an unwind drops mid-flight kills
/// the pgid, which must still be the anchor's — a reaped leader's pid may be
/// reused once its group is empty.
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
        let plan = resolve_pipeline(stages, env, eval, mooring, shell)?;

        // Window start for the sandbox-denial reader.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();

        // A joined group never prepares a loan: dropping one restores the
        // terminal, out from under the owning pipeline.
        let (group, loan) = if let Some(membership) = shell.io.stage.as_ref() {
            (PipelineGroup::joining(membership.clone()), None)
        } else {
            // Capture before even the anchor can run; stages share this snapshot.
            let prepared = (plan.terminal == TerminalPlan::ForegroundExternalGroup)
                .then(|| shell.terminal_lease(mooring))
                .flatten()
                .and_then(|lease| crate::process::TerminalLoan::prepare(lease, &mooring.home));
            let group = PipelineGroup::prepare(shell, tx.clone())?;
            let loan = prepared.and_then(|loan| loan.lend(group.leader_pgid().as_raw()));
            (group, loan)
        };

        let collect = CollectState::new(
            rx,
            group.group(),
            loan,
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
            holds_terminal: node.collect.holds_terminal(),
        };
        let launched = launch(stages, plan.specs, &mut cx, &mut node.collect, &tx);
        // The collector keeps no sender: `recv` sees the last producer go.
        drop(tx);
        if launched.is_err() {
            node.collect.roll_back();
        }
        let value = node.join(mooring, shell);
        launched.and(value)
    }

    /// Wait on every stage and fold their observations into one value.  The
    /// anchor ends before the fold, so every key it heard is on the channel;
    /// safe, `drive` returning only with no stage left to address the pgid.
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
    specs: Vec<StageSpec>,
    cx: &mut LaunchCx<'_>,
    collect: &mut CollectState,
    tx: &Sender<Event>,
) -> Settled<()> {
    let routes = open_stage_routes(stages.len())?;
    for (ix, ((stage, spec), route)) in stages.iter().zip(specs).zip(routes).enumerate() {
        cx.shell.poll()?;
        let handle = spawn_stage(stage, spec, route, cx, Slot { ix, tx: tx.clone() })?;
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
        use super::stage::StageHandle;
        use super::{CollectState, PipeNode, PipelineGroup, Slot};
        use crate::shell::runtime::Mooring;

        let trapper = Trapper::new();
        let mut shell = crate::test_helper::core_shell();
        let mooring = Mooring::adrift();
        let frame = crate::process::DurableRoot::default().worker();
        let (tx, rx) = std::sync::mpsc::channel();
        let group = PipelineGroup::prepare(&shell, tx.clone()).expect("anchor spawns");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", &trapper.script()]);
        let (child, _) = crate::process::spawn_with_pgid(
            &mut cmd,
            crate::process::PgidPolicy::Join(group.leader_pgid()),
        )
        .expect("spawn the trapper into the group");
        let mut collect = CollectState::new(
            rx,
            group.group(),
            Some(crate::process::TerminalLoan::for_test(&frame)),
            shell.scope.clone(),
            mooring.cleanup_grace,
            std::time::Instant::now(),
        );
        collect.push(StageHandle::for_test(
            Slot { ix: 0, tx },
            crate::process::ChildHandle::from_std(child),
        ));
        let pid = trapper.pid();
        // As the tty delivers a key: to the whole group, the anchor included.
        group
            .leader_pgid()
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
    }
}
