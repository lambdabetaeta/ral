//! Pipeline execution engine.  `resolve` freezes the terminal handoff and each
//! stage's launch decision, `launch` places every stage in one process group,
//! `join` folds their observations into one value.

mod collect;
mod group;
pub(crate) mod helper;
mod launch;
pub(crate) mod resolve;
mod route;
mod sentinel;
mod stage;
mod thread;

use crate::ir::{Comp, PipeYield};
use crate::types::{Env, Mooring, Settled, Shell, Value};
use std::sync::Arc;
use std::time::Instant;

use collect::{CollectState, Slot};
use group::PipelineGroup;
use launch::{LaunchCx, spawn_stage};
use resolve::{TerminalPlan, resolve_pipeline};
use route::open_stage_routes;

/// A multi-stage pipeline between its launch and its join, its stages running
/// in their own process group.
///
/// Field order is teardown order: a collector dropped mid-flight kills the
/// pgid, which must still be the anchor's — a reaped leader's pid may be
/// reused once its group is empty.
pub(crate) struct PipeNode {
    collect: CollectState,
    group: PipelineGroup,
    yields: PipeYield,
}

impl PipeNode {
    /// Resolve the plan and spawn every stage into one process group, none
    /// observed yet.  No stage runs in the parent, so none is in tail
    /// position.
    ///
    /// `env` is the pipeline node's own lexical environment — the machine's
    /// `E` in focus, not necessarily `shell.env` — and is what a stage
    /// thread's closure captures.
    pub(crate) fn launch(
        stages: &[Arc<Comp>],
        yields: PipeYield,
        env: &Env,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        crate::process::check(mooring)?;
        let plan = resolve_pipeline(stages, env, mooring, shell)?;

        // Window start for the sandbox-denial reader.
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();

        let group = match shell.io.launch_role.membership() {
            Some(membership) => PipelineGroup::joining(membership.clone()),
            None => PipelineGroup::prepare(shell, tx.clone())?,
        };
        let loan = (plan.terminal == TerminalPlan::ForegroundExternalGroup)
            .then(|| group.lend(shell, mooring))
            .flatten();

        let collect = CollectState::new(rx, &tx, group.group(), loan, mooring, started);
        let mut node = Self {
            collect,
            group,
            yields,
        };
        // Declared after `node` so unconsumed routes close before it tears
        // down: a half-wired neighbour must see EOF.
        let routes = open_stage_routes(stages.len())?;

        let mut cx = LaunchCx {
            mooring,
            shell,
            env,
            group: &node.group,
            holds_terminal: node.collect.holds_terminal(),
        };
        for (ix, ((stage, spec), route)) in stages.iter().zip(&plan.specs).zip(routes).enumerate() {
            crate::process::check(mooring)?;
            let handle = spawn_stage(stage, spec, route, &mut cx, Slot { ix, tx: tx.clone() })?;
            node.collect.push(handle);
        }
        Ok(node)
    }

    /// Wait on every stage and fold their observations into one value.  The
    /// anchor ends before the fold, so every key it heard is on the channel;
    /// safe, `drive` returning only with no stage left to address the pgid.
    pub(crate) fn join(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let Self {
            mut collect,
            mut group,
            yields,
        } = self;
        collect.drive();
        group.end_anchor();
        let value = collect.fold(mooring, shell, yields);
        drop(group);
        value
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use crate::process::{CancelCause, ForegroundScope};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

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

        fn command(&self) -> String {
            format!(
                "sh -c 'trap \"echo x >> {}\" INT TERM; echo $$ > {}; while :; do sleep 0.05; done'",
                self.path("hits").display(),
                self.path("ready").display(),
            )
        }

        fn pid(&self) -> i32 {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let read = std::fs::read_to_string(self.path("ready")).unwrap_or_default();
                if let Some(pid) = read.strip_suffix('\n').and_then(|p| p.parse().ok()) {
                    return pid;
                }
                assert!(Instant::now() < deadline, "the trapper never set its trap");
                std::thread::sleep(Duration::from_millis(10));
            }
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
    fn run_struck(src: &str, trapper: &Trapper, strike: impl FnOnce(&ForegroundScope, i32) + Send) {
        let mut shell = crate::types::Shell::new(crate::io::TerminalState::default());
        let scope = shell.run_cancel_handle();
        std::thread::scope(|s| {
            s.spawn(|| strike(&scope, trapper.pid()));
            let _ = shell.run_under(
                &scope,
                crate::run::RunRequest {
                    run: crate::engine::testkit::run(src),
                    surface: None,
                    deferred: None,
                    desk: None,
                    fork: None,
                },
            );
        });
    }

    /// The owner's group signal is the member's one delivery: its own waiter
    /// sends no second copy.
    #[test]
    fn a_struck_frame_signals_a_stage_member_once() {
        let trapper = Trapper::new();
        run_struck(
            &format!("!{{ {} }} | cat", trapper.command()),
            &trapper,
            |scope, _| {
                scope.cancel(CancelCause::Interrupt);
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
            &format!("!{{ {} }} | cat", trapper.command()),
            &trapper,
            |scope, pid| {
                let pid = rustix::process::Pid::from_raw(pid).expect("a live pid");
                let pgid = rustix::process::getpgid(Some(pid)).expect("the trapper's group");
                // Past the window between the anchor's exec and its handler install.
                std::thread::sleep(Duration::from_millis(300));
                unsafe { libc::kill(-pgid.as_raw_nonzero().get(), libc::SIGINT) };
                let deadline = Instant::now() + Duration::from_secs(5);
                while trapper.hits() == 0 {
                    assert!(Instant::now() < deadline, "the trapper never heard SIGINT");
                    std::thread::sleep(Duration::from_millis(10));
                }
                // Room for a second copy, were anything to send one.
                std::thread::sleep(Duration::from_millis(300));
                scope.cancel(CancelCause::RootAbort);
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
            &format!("!{{ {} | cat }} | cat", trapper.command()),
            &trapper,
            |scope, _| scope.cancel(CancelCause::Deadline),
        );
        assert_eq!(trapper.hits(), 1);
    }
}
