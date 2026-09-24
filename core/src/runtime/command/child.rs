//! In-flight child handles: [`RunningChild`] after spawn, [`WaitedChild`] after
//! the wait, and [`Pumps`], planned as sinks before the spawn and started as
//! drainers after it.  The running → waited → settled typestate makes "settle
//! before wait" and "wait twice" unwritable.

use crate::io::Sink;
use crate::process::{CancelCause, Group};

/// A child's stdout/stderr pumps: planned as the [`Sink`] each drains into,
/// `None` where that fd is inherited or wired straight to an OS pipe, then
/// started as the drainer threads over the child's pipes.  A reader-gone kill's
/// remaining bytes are owed to nobody, and a descendant it missed could hold the
/// pipe for ever, so that ending detaches; every other joins, after the kill
/// that frees the pipe.
pub(crate) struct Pumps<T = std::thread::JoinHandle<()>> {
    stdout: Option<T>,
    stderr: Option<T>,
}

impl<T> Default for Pumps<T> {
    fn default() -> Self {
        Self {
            stdout: None,
            stderr: None,
        }
    }
}

impl Pumps<Sink> {
    pub(crate) const fn new(stdout: Option<Sink>, stderr: Option<Sink>) -> Self {
        Self { stdout, stderr }
    }

    /// Start the planned drainers over `child`'s piped stdout/stderr.
    pub(crate) fn start(self, child: &mut crate::process::ChildHandle) -> Pumps {
        Pumps {
            stdout: self
                .stdout
                .and_then(|sink| child.take_stdout().map(|s| sink.pump(s))),
            stderr: self
                .stderr
                .and_then(|sink| child.take_stderr().map(|s| sink.pump(s))),
        }
    }
}

impl Pumps {
    /// Join both drainers, or detach them when `detach`.
    pub(crate) fn settle(self, detach: bool) {
        if detach {
            return;
        }
        if let Some(jh) = self.stdout {
            let _ = jh.join();
        }
        if let Some(jh) = self.stderr {
            let _ = jh.join();
        }
    }
}

/// What `RunningChild::wait` hears from the reaper's `Watch` or from a
/// cancel: the wire type for its own single-child fold.
enum ChildEvent {
    Ended(crate::process::WaitOutcome),
    Cancelled(CancelCause),
}

/// A spawned external child, watched by the reaper, plus the threads
/// draining its piped stdout/stderr; the shared core of standalone exec and
/// pipeline external stages.
///
/// There is no `RunningChild::drain`, so "join the pumps while the pipe is
/// still open" has no spelling; `wait` consumes self, so neither does "wait
/// twice".  The `Option` around `watch` is `Drop`'s disarm latch.  Holding the
/// pgid rather than the pid means the abort path's SIGKILL reaches
/// descendants, but only where the child owns its group: a pipeline's group
/// is its owner's to address as a whole.
///
/// Audit-agnostic: byte capture belongs to the caller.
pub(crate) struct RunningChild {
    watch: Option<crate::process::Watch>,
    events: std::sync::mpsc::Receiver<ChildEvent>,
    tx: std::sync::mpsc::Sender<ChildEvent>,
    /// The group this child landed in: `Owns` is released on Windows and
    /// signalled or killed whole; `None` shares ral's own group, addressed by
    /// pid.
    group: Option<Group>,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.  Teardown
    /// prefers it over the pgid: a grandchild that `setsid()`'d away escapes
    /// `kill(-pgid, …)` but cannot leave its cgroup.
    jail: Option<crate::process::jail::JailCgroup>,
    pumps: Pumps,
    name: String,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pid: u32,
    /// Polled through `watch_cancel` in `wait`, so a cancel — exarch's tool
    /// timeout, a signal the platform handler translated into a cause — can
    /// preempt a child that never exits on its own.
    cancel: crate::process::CancelScope,
    /// The strongest cause in force when the child ended: the sole input to
    /// attribution, forgiveness, and whether the drainers are joined.
    cause: Option<CancelCause>,
}

/// A child observed dead, holding its outcome and its not-yet-joined drainers.
/// [`RunningChild::wait`] is the only constructor, so atomic-redirect commit and
/// status interpretation carry a borrow-check proof that the child has exited.
pub(crate) struct WaitedChild {
    pub(crate) outcome: crate::process::WaitOutcome,
    pub(crate) cause: Option<CancelCause>,
    pumps: Pumps,
    /// Trace context carried from the `RunningChild` so `settle`'s pump-join
    /// timings attribute to the same command instance.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    name: String,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pid: u32,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    t_enter: std::time::Instant,
}

impl RunningChild {
    /// The one place a `RunningChild` is built: [`super::run`] and the stage
    /// launcher in `runtime::pipeline::launch` both come here, so the wait and
    /// `Drop` rules are never re-derived per call site.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single assembly point for every field RunningChild carries; splitting it would just scatter the same parameters across a builder"
    )]
    pub(crate) fn assemble_with_owner(
        child: crate::process::ChildHandle,
        name: String,
        pumps: Pumps<Sink>,
        group: Option<Group>,
        cancel: crate::process::CancelScope,
        jail: Option<crate::process::jail::JailCgroup>,
    ) -> Self {
        let mut child = child;
        let pid = child.id();
        let pumps = pumps.start(&mut child);
        let (tx, events) = std::sync::mpsc::channel();
        let watch = child.into_watch(tx.clone(), ChildEvent::Ended);
        Self {
            watch: Some(watch),
            events,
            tx,
            group,
            jail,
            pumps,
            name,
            pid,
            cancel,
            cause: None,
        }
    }

    /// SIGKILL the process group this child owns outright, or the child alone
    /// via its `Watch` — [`Self::group`] says which.  Idempotent, so
    /// [`Self::wait`]'s cancel branch and [`Drop`] need not coordinate.
    ///
    /// A tracked jail cgroup wins over the pgid: `cgroup.kill` reaches a
    /// grandchild that `setsid()`'d out of the group, and the jail's
    /// unprivileged uid cannot write `cgroup.procs` to escape.
    fn kill_group(&self, watch: &crate::process::Watch) {
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = &self.jail {
            crate::process::jail::linux::kill(cgroup);
            return;
        }
        match &self.group {
            Some(Group::Owns(g)) => g.kill(),
            Some(Group::Joins(_)) | None => watch.kill(),
        }
    }

    /// Cancel-path teardown: open with the cause's [`grace_signal`], grace
    /// briefly, then kill regardless — and where the cause offers none
    /// (reader-gone, root abort), or nothing takes it, kill outright.
    /// `Some(outcome)` means the grace wait already caught the exit and the
    /// caller must not wait again.
    ///
    /// The final kill goes through [`Self::kill_group`], where `cgroup.kill`
    /// catches the grandchild that `setsid()`'d out of the signal's reach.
    ///
    /// [`grace_signal`]: crate::process::grace_signal
    fn terminate(
        &self,
        watch: &crate::process::Watch,
        cause: CancelCause,
    ) -> Option<crate::process::WaitOutcome> {
        let graced = crate::process::grace_signal(cause)
            .is_some_and(|signal| self.open_teardown(watch, signal, cause));
        let reaped = graced
            .then(
                || match self.events.recv_timeout(crate::process::TEARDOWN_GRACE) {
                    Ok(ChildEvent::Ended(o)) => Some(o),
                    Ok(ChildEvent::Cancelled(_)) | Err(_) => None,
                },
            )
            .flatten();
        // Idempotent on an already-reaped child, so this always runs —
        // harmless on a tree that already left, decisive against a
        // grandchild that trapped the signal and still holds the pipe.
        self.kill_group(watch);
        reaped
    }

    /// Send `signal` where it is this waiter's to send — a pipeline member
    /// only a cause its owner does not deliver — and say whether a grace
    /// follows.
    #[cfg(unix)]
    fn open_teardown(
        &self,
        watch: &crate::process::Watch,
        signal: crate::process::Signal,
        cause: CancelCause,
    ) -> bool {
        match &self.group {
            Some(Group::Owns(g)) => g.signal_group(signal),
            Some(Group::Joins(m)) if !m.owes(cause) => {}
            Some(Group::Joins(_)) | None => watch.signal(signal),
        }
        true
    }

    /// Only an owned console group takes Ctrl-Break, from its owner: a member
    /// not owed the cause waits out the owner's grace, and a pid gets the kill
    /// alone — an `Inherit` child already heard the console's own Ctrl-C.
    #[cfg(windows)]
    fn open_teardown(
        &self,
        _: &crate::process::Watch,
        _: crate::process::Signal,
        cause: CancelCause,
    ) -> bool {
        match &self.group {
            Some(Group::Owns(g)) => {
                crate::process::break_pipeline_group(*g);
                true
            }
            Some(Group::Joins(m)) => !m.owes(cause),
            None => false,
        }
    }
}

impl RunningChild {
    /// Wait for the child to terminate, consuming `self`; the returned
    /// `WaitedChild` is from here on the only handle on the drainer threads.
    ///
    /// One `recv`, no poll and no sleep: the reaper posts the exit and a
    /// cancel arrives the same way.  A stop never reaches here — the reaper
    /// answers it with `SIGCONT` itself, the one rule in one place.
    pub(crate) fn wait(mut self) -> WaitedChild {
        // Taking the watch disarms `Drop` for the success path.
        let watch = self.watch.take().expect("RunningChild has no watch");
        let pid = self.pid;
        let t_enter = std::time::Instant::now();
        crate::dbg_trace!("wait", "enter name={} pid={}", self.name, pid);

        let tx = self.tx.clone();
        let _cancel = crate::process::watch_cancel(self.cancel.clone(), move |cause| {
            let _ = tx.send(ChildEvent::Cancelled(cause));
        });

        let outcome = match self.events.recv().expect("tx outlives this recv") {
            ChildEvent::Ended(o) => {
                // A cause may land after the child's death but before this read.
                self.cause = self.cause.max(self.cancel.cause());
                o
            }
            ChildEvent::Cancelled(cause) => {
                crate::dbg_trace!(
                    "wait",
                    "cancel name={} pid={} cause={cause:?}",
                    self.name,
                    pid
                );
                self.cause = Some(cause);
                match self.terminate(&watch, cause) {
                    Some(o) => o,
                    None => loop {
                        if let ChildEvent::Ended(o) =
                            self.events.recv().expect("tx outlives this recv")
                        {
                            break o;
                        }
                    },
                }
            }
        };
        crate::dbg_trace!(
            "wait",
            "ended name={} pid={} elapsed={:?} outcome={:?}",
            self.name,
            pid,
            t_enter.elapsed(),
            outcome,
        );
        // Let the Job Object's whole-job completion drain any descendants before
        // the handle goes.  A pipeline stage never lands here: its release
        // belongs to `PipelineGroup::Drop`.
        #[cfg(windows)]
        if let Some(Group::Owns(group)) = self.group {
            let _ = crate::process::wait_leader_blocking(group);
            crate::process::release_win_group(group.as_raw());
        }
        // The leader is dead here, so finish the cgroup lest a straggler
        // outlive the command.  Windows releases its own group bookkeeping at
        // this same point.
        if let Some(jail) = &self.jail {
            jail.finish();
        }
        let _ = watch.reap();
        WaitedChild {
            outcome,
            cause: self.cause,
            pumps: std::mem::take(&mut self.pumps),
            name: self.name.clone(),
            pid,
            t_enter,
        }
    }
}

impl WaitedChild {
    /// Join the drainer threads — or, for a child ral killed because its
    /// reader was gone, detach them; see [`Pumps::settle`].
    pub(crate) fn settle(self) {
        let detach = self.cause == Some(CancelCause::ReaderGone);
        crate::dbg_trace!(
            "wait",
            "drain-begin name={} pid={} elapsed={:?} detach={detach}",
            self.name,
            self.pid,
            self.t_enter.elapsed(),
        );
        self.pumps.settle(detach);
        crate::dbg_trace!(
            "wait",
            "drain-end name={} pid={} elapsed={:?}",
            self.name,
            self.pid,
            self.t_enter.elapsed(),
        );
    }
}

impl Drop for RunningChild {
    /// Abort path: SIGKILL the group, join the drainers, reap.  A no-op once
    /// `wait` has taken the watch — that is the success path's disarm.  The
    /// kill runs before the pump join it frees: killing first is what makes
    /// the joins terminate.
    fn drop(&mut self) {
        let Some(watch) = self.watch.take() else {
            return;
        };
        self.kill_group(&watch);
        #[cfg(windows)]
        if let Some(Group::Owns(group)) = self.group {
            crate::process::release_win_group(group.as_raw());
        }
        #[cfg(target_os = "linux")]
        if let Some(jail) = &self.jail {
            crate::process::jail::linux::remove(jail);
        }
        // The abort path always joins, never detaches: nothing has decided
        // this child's remaining bytes are owed to nobody.
        std::mem::take(&mut self.pumps).settle(false);
        // `Watch::drop` reaps.
        drop(watch);
    }
}

#[cfg(unix)]
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::process::*;

    /// A wall that expires mid-command is reported as the time limit it was,
    /// not as the SIGTERM we happened to send.
    #[test]
    fn a_deadline_teardown_reports_the_time_limit_not_the_signal() {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("30");
        let (child, pgid) = spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader)
            .expect("spawn /bin/sleep under a pgid");

        let scope = CancelScope::root();
        let running = RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            "sleep".to_string(),
            Pumps::default(),
            Some(Group::Owns(pgid.expect("NewLeader yields a tracked pgid"))),
            scope.clone(),
            None,
        );
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Deadline);
        });

        let waited = running.wait();
        let end = waited.outcome.classify(waited.cause, false);
        waited.settle();
        canceller.join().expect("canceller thread");

        assert_eq!(end, Some(ChildEnd::Cancelled(CancelCause::Deadline)));
    }

    /// Run `sh` under a scope struck with `cause` — before the dispatch when
    /// `early`, else once the child has proved itself running — and return the
    /// error the run raised.
    fn struck(cause: CancelCause, early: bool) -> crate::types::Error {
        let dir = tempfile::tempdir().expect("a temp dir");
        let marker = dir.path().join("running");
        let mut shell = crate::types::Shell::new(crate::io::TerminalState::default());
        let scope = shell.run_cancel_handle();
        if early {
            scope.cancel(cause);
        }
        let canceller = (!early).then(|| {
            let (scope, marker) = (scope.clone(), marker.clone());
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !marker.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the child never touched its marker"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                scope.cancel(cause);
            })
        });
        let src = format!("sh -c 'touch {}; exec sleep 30'", marker.display());
        let report = shell.run_under(
            &scope,
            crate::run::RunRequest {
                run: crate::engine::testkit::run(&src),
                surface: None,
                deferred: None,
                desk: None,
                fork: None,
            },
        );
        if let Some(canceller) = canceller {
            canceller.join().expect("canceller thread");
        }
        match report {
            crate::run::RunReport::Ran {
                ending:
                    crate::run::Ending::Raised { error, .. } | crate::run::Ending::Walled { error, .. },
                ..
            } => error,
            _ => panic!("a struck run must raise"),
        }
    }

    /// A run's failure is a function of its cause, never of where the cancel
    /// landed: before the dispatch, a poll point reports it; during it, the
    /// child's teardown does — and the two must agree.
    #[test]
    fn a_cancellation_reports_alike_before_and_during_a_command() {
        for cause in [
            CancelCause::Interrupt,
            CancelCause::Explicit,
            CancelCause::Deadline,
            CancelCause::Terminate,
            CancelCause::RootAbort,
        ] {
            let (before, during) = (struck(cause, true), struck(cause, false));
            assert_eq!(before.exit_code(), during.exit_code(), "{cause:?}: code");
            assert_eq!(
                crate::evaluator::scope::reason_value(&before.status),
                crate::evaluator::scope::reason_value(&during.status),
                "{cause:?}: reason"
            );
            assert_eq!(before.message, during.message, "{cause:?}: message");
            assert_eq!(before.hint, during.hint, "{cause:?}: hint");
        }
    }

    /// An interrupt's SIGINT grace must not become a wait on the child's own
    /// 30 s sleep, and the follow-up group SIGKILL must reap a grandchild the
    /// SIGINT never reached.
    #[test]
    fn interrupt_tears_down_external_subprocess_tree() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped());

        let (mut child, pgid) =
            spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader).expect("spawn /bin/sh under new pgid");
        assert!(pgid.is_some(), "NewLeader yields a tracked pgid");

        // Take stdout before assembling: with no stdout sink planned the
        // `ChildHandle` would keep it attached.  `echo $!` flushes at startup,
        // so the reader thread returns long before teardown.
        let stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::BufReader::new(stdout).read_line(&mut line).ok();
            line
        });

        let scope = CancelScope::root();
        let running = RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            "sh".to_string(),
            Pumps::default(),
            Some(Group::Owns(pgid.expect("NewLeader yields a tracked pgid"))),
            scope.clone(),
            None,
        );

        // `wait` is a single blocking `recv`, so any delay past the sleep
        // still lands well inside it.
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Interrupt);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        waited.settle();
        canceller.join().expect("canceller thread");

        // Property 1: grace is 500 ms then group SIGKILL, so real time is well
        // under a second; 10 s is a ceiling far below the 30 s sleep.
        assert!(
            elapsed.as_secs() < 10,
            "interrupt teardown must not block on the child's own sleep: returned after {elapsed:?}"
        );

        // Property 2: `kill(pid, 0)` returns ESRCH once the grandchild is gone;
        // poll to absorb the window between the SIGKILL and the kernel reaping.
        let line = reader.join().expect("reader thread");
        let gc_pid: i32 = line
            .trim()
            .parse()
            .expect("the grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            if rustix::process::test_kill_process(rustix::process::Pid::from_raw(gc_pid).unwrap())
                .is_err()
            {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the forked grandchild (pid {gc_pid}) survived the interrupt teardown"
        );
    }

    /// A stop is answered with `SIGCONT` at once by the reaper — the one
    /// rule, with no owner above it and nothing tracking the stop.
    #[test]
    fn wait_revives_an_ownerless_sigstopped_child() {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("0.2");
        let (child, pgid) = spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader)
            .expect("spawn /bin/sleep under a pgid");
        let pgid = pgid.expect("NewLeader yields a tracked pgid");
        rustix::process::kill_process(pgid.as_pid(), rustix::process::Signal::STOP)
            .expect("SIGSTOP the sleep");

        let running = RunningChild::assemble_with_owner(
            crate::process::ChildHandle::from_std(child),
            "sleep".to_string(),
            Pumps::default(),
            Some(Group::Owns(pgid)),
            CancelScope::root(),
            None,
        );

        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        waited.settle();

        assert!(
            elapsed.as_secs() < 5,
            "a SIGSTOP'd child must be revived rather than hung: took {elapsed:?}"
        );
    }

    /// A pipeline member's waiter opens only with a cause struck beneath its
    /// stage: what the stage holds is the owner's to deliver, so striking the
    /// stage leaves the member to the final kill.
    #[test]
    fn a_members_waiter_signals_only_what_its_owner_does_not_deliver() {
        for (strike_stage, expected) in [(false, 1), (true, 0)] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let (hits, ready) = (dir.path().join("hits"), dir.path().join("ready"));
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args([
                "-c",
                &format!(
                    "trap 'echo x >> {}' INT TERM; : > {}; while :; do sleep 0.05; done",
                    hits.display(),
                    ready.display()
                ),
            ]);
            let (child, pgid) =
                spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader).expect("spawn the trapper");

            let stage = CancelScope::root();
            let own = stage.child();
            let running = RunningChild::assemble_with_owner(
                crate::process::ChildHandle::from_std(child),
                "sh".to_string(),
                Pumps::default(),
                Some(Group::Joins(Membership::new(
                    pgid.expect("NewLeader yields a tracked pgid"),
                    stage.clone(),
                ))),
                own.clone(),
                None,
            );
            let striker = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !ready.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the trapper never set its trap"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let struck = if strike_stage { stage } else { own };
                struck.cancel(CancelCause::Deadline);
            });

            let waited = running.wait();
            striker.join().expect("striker thread");
            let outcome = waited.outcome;
            waited.settle();

            let heard = std::fs::read_to_string(&hits)
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(heard, expected, "strike_stage={strike_stage}");
            assert_eq!(
                outcome,
                WaitOutcome::Signaled(Signal::new(libc::SIGKILL)),
                "strike_stage={strike_stage}: the trapper outlives its grace"
            );
        }
    }
}
