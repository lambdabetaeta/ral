//! In-flight child handles: [`RunningChild`] after spawn, [`WaitedChild`] after
//! the wait, and [`ExternalPlumbing`], the pump plan the caller hands in.  The
//! running → waited → settled typestate makes "settle before wait" and "wait
//! twice" unwritable.

use crate::io::Sink;
use crate::process::{CancelCause, Ending, EndingCell, KillTarget};
use crate::types::{Break, Error, Settled};

/// Who releases the Windows group registered in `win_groups`
/// (`process::signal::windows`), which unlike a Unix pgid does not vanish when
/// its members exit.  `Standalone` releases its own once `wait` returns and
/// carries the pgid to do it with; `BorrowedByPipeline` leaves it to
/// `PipelineGroup` in `runtime::pipeline`, so a `RunningChild::Drop` racing
/// that owner's `Drop` cannot double-close, and needs no pgid of its own;
/// `None` is a child spawned with `PgidPolicy::Inherit` and has no group at
/// all.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GroupOwner {
    None,
    Standalone(crate::process::Pgid),
    BorrowedByPipeline,
}

/// A spawned external child plus the threads draining its piped stdout/stderr;
/// the shared core of standalone exec and pipeline external stages.
///
/// There is no `RunningChild::drain`, so "join the pumps while the pipe is
/// still open" has no spelling; `wait` consumes self, so neither does "wait
/// twice".  The `Option` around `child` is `Drop`'s disarm latch: `wait` takes
/// it and never puts it back, so the abort path short-circuits once `wait` ran.
/// Holding the pgid rather than the pid means that abort-path SIGKILL reaches
/// descendants — `/bin/sh -c 'sleep 999'` leaves no orphan behind — but only
/// for `GroupOwner::Standalone`, the only variant a kill may address by group.
///
/// Audit-agnostic: byte capture belongs to the caller.  A standalone external
/// is teed at dispatch level by `evaluator::with_audit_capture`; a direct-spawn
/// pipeline stage writes into the next stage's pipe and gets a synthesised node
/// with empty stdout from `runtime::pipeline::collect`.
pub(crate) struct RunningChild {
    pub child: Option<crate::process::ChildHandle>,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.  Teardown
    /// prefers it over the pgid: a grandchild that `setsid()`'d away escapes
    /// `kill(-pgid, …)` but cannot leave its cgroup.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub jail: Option<crate::process::jail::JailCgroup>,
    pub pump: Option<std::thread::JoinHandle<()>>,
    /// `None` when stderr was inherited, redirected to a file, or dup'd onto
    /// stdout by `2>&1` — nothing to pump in any of those cases.
    pub stderr_pump: Option<std::thread::JoinHandle<()>>,
    pub name: String,
    pub group_owner: GroupOwner,
    /// Polled by `wait`, because a blocking `waitpid` / `WaitForSingleObject`
    /// consults nothing: without the poll an upstream cancel (exarch's tool
    /// timeout, a signal the platform handler translated into a cause) could
    /// not preempt a child that never exits on its own.
    pub cancel: crate::process::CancelScope,
    /// How this child's life ended, once ral itself ended it: the cancel
    /// branch of `wait` and the pipeline collector's reader-gone kill are the
    /// only writers.  Sole input to forgiveness and to whether the drainers
    /// are joined.
    ending: EndingCell,
}

/// A child observed dead, holding its outcome and its not-yet-joined drainers.
/// [`RunningChild::wait`] is the only constructor, so atomic-redirect commit and
/// status interpretation carry a borrow-check proof that the child has exited.
pub(crate) struct WaitedChild {
    pub outcome: crate::process::WaitOutcome,
    pub ending: Ending,
    pump: Option<std::thread::JoinHandle<()>>,
    stderr_pump: Option<std::thread::JoinHandle<()>>,
    /// Trace context carried from the `RunningChild` so `settle`'s pump-join
    /// timings attribute to the same command instance.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    name: String,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pid: u32,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    t_enter: std::time::Instant,
}

/// Where to pump a child's stdout / stderr.  Either field is `None` when that
/// fd was inherited or wired straight to an OS pipe (the next stage's stdin).
pub(crate) struct ExternalPlumbing {
    pub stdout_pump: Option<Sink>,
    pub stderr_pump: Option<Sink>,
}

impl RunningChild {
    /// The one place a `RunningChild` is built: [`super::run`] and the stage
    /// launcher in `runtime::pipeline::launch` both come here, so the wait and
    /// `Drop` rules are never re-derived per call site.  See [`GroupOwner`] for
    /// what `group_owner` obliges each of them to.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single assembly point for every field RunningChild carries; splitting it would just scatter the same parameters across a builder"
    )]
    pub(crate) fn assemble_with_owner(
        child: crate::process::ChildHandle,
        name: String,
        plumbing: ExternalPlumbing,
        group_owner: GroupOwner,
        cancel: crate::process::CancelScope,
        jail: Option<crate::process::jail::JailCgroup>,
    ) -> Self {
        let mut child = child;
        let ExternalPlumbing {
            stdout_pump,
            stderr_pump,
        } = plumbing;
        let pump = stdout_pump.and_then(|sink| child.take_stdout().map(|s| sink.pump(s)));
        let stderr_pump = stderr_pump.and_then(|sink| child.take_stderr().map(|s| sink.pump(s)));
        Self {
            child: Some(child),
            jail,
            pump,
            stderr_pump,
            name,
            group_owner,
            cancel,
            ending: EndingCell::default(),
        }
    }

    /// Who a signal this child sends addresses: its whole group, iff it owns
    /// one outright, else its own pid alone.  A stage borrowing a pipeline's
    /// group (`BorrowedByPipeline`) may never address that group — only the
    /// owning `PipelineGroup` may.
    fn kill_target(&self) -> KillTarget {
        match self.group_owner {
            GroupOwner::Standalone(p) => KillTarget::Group(p),
            GroupOwner::BorrowedByPipeline | GroupOwner::None => KillTarget::Pid,
        }
    }

    /// SIGKILL the process group this child owns outright, or the child alone
    /// — [`Self::kill_target`] says which.  Idempotent on both platforms, so
    /// [`Self::wait`]'s cancel branch and [`Drop`] may each call it without
    /// coordinating.  It does not release the Windows group bookkeeping:
    /// `wait` does that after `wait_leader_blocking`, `Drop` inline.
    ///
    /// A tracked jail cgroup wins over the pgid, because `cgroup.kill` reaches a
    /// grandchild that `setsid()`'d out of the group and the jail's
    /// unprivileged uid cannot write `cgroup.procs` to escape.
    fn kill_group(&self, child: &mut crate::process::ChildHandle) {
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = &self.jail {
            crate::process::jail::linux::kill(cgroup);
            return;
        }
        #[cfg(unix)]
        match self.kill_target() {
            KillTarget::Group(group) => {
                let _ = rustix::process::kill_process_group(
                    group.as_pid(),
                    rustix::process::Signal::KILL,
                );
            }
            KillTarget::Pid => {
                let _ = child.kill();
            }
        }
        #[cfg(windows)]
        match self.kill_target() {
            KillTarget::Group(p) => {
                crate::process::kill_pipeline_group(p);
            }
            KillTarget::Pid => {
                let _ = child.kill();
            }
        }
    }

    /// Poll for the leader until `deadline`, answering a stop inline with
    /// `SIGCONT`; `None` on timeout.
    #[cfg(unix)]
    fn grace_poll(
        child: &mut crate::process::ChildHandle,
        deadline: std::time::Instant,
    ) -> Option<crate::process::WaitOutcome> {
        let pid = child.id();
        while std::time::Instant::now() < deadline {
            match child.try_wait_handling_stop() {
                Ok(Some(crate::process::WaitPoll::Stopped(_))) => {
                    crate::process::cont_stage_by_pid(pid);
                    continue;
                }
                Ok(Some(crate::process::WaitPoll::Done(o))) => return Some(o),
                Err(_) => break,
                Ok(None) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    /// Cancel-path teardown: signal the group by cause — SIGINT for an
    /// interrupt, SIGTERM for a cancel, deadline, or termination request,
    /// straight SIGKILL for a root abort — grace briefly, then kill regardless.
    /// Signalling the group rather than the leader also takes out forked
    /// grandchildren, closing the stdout pipe the pumps are waiting on.
    ///
    /// `Some(outcome)` means the grace peek already reaped the leader and the
    /// caller must not wait again; `None` leaves the reaping to the caller's
    /// blocking `wait_handling_stop`.
    ///
    /// The grace signal addresses the same target [`Self::kill_group`] would
    /// kill — a grandchild that `setsid()`'d away would miss it either way —
    /// but the final kill goes through [`Self::kill_group`], where
    /// `cgroup.kill` does catch it.
    fn terminate_group(
        &self,
        child: &mut crate::process::ChildHandle,
        cause: crate::process::CancelCause,
    ) -> Option<crate::process::WaitOutcome> {
        #[cfg(unix)]
        {
            // A root abort skips the grace ladder outright, addressed or not.
            if cause == crate::process::CancelCause::RootAbort {
                self.kill_group(child);
                return None;
            }
            let signal = if cause == crate::process::CancelCause::Interrupt {
                rustix::process::Signal::INT
            } else {
                rustix::process::Signal::TERM
            };
            match self.kill_target() {
                KillTarget::Pid => {
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps"
                    )]
                    let _ = rustix::process::kill_process(
                        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
                        signal,
                    );
                }
                KillTarget::Group(pgid) => {
                    let _ = rustix::process::kill_process_group(pgid.as_pid(), signal);
                }
            }
            let deadline = std::time::Instant::now() + crate::process::TEARDOWN_GRACE;
            let reaped = Self::grace_poll(child, deadline);
            // `Child::kill` on an already-reaped child is an already-ignored
            // error, so this always runs — harmless on a tree that already
            // left, decisive against a grandchild that trapped the signal and
            // still holds the pipe.
            self.kill_group(child);
            reaped
        }
        #[cfg(not(unix))]
        {
            let _ = cause;
            self.kill_group(child);
            None
        }
    }
}

impl RunningChild {
    /// Wait for the child to terminate, consuming `self`; the returned
    /// `WaitedChild` is from here on the only handle on the drainer threads.
    ///
    /// Every stop is answered with `SIGCONT` at once and the wait resumes on
    /// the same child — this method never returns while the child is merely
    /// stopped.  `Drop`'s kill is already disarmed by then.
    pub fn wait(mut self) -> Settled<WaitedChild> {
        // Taking the child disarms `Drop` for the success path.
        let mut child = self.child.take().expect("RunningChild has no child");
        let pid = child.id();
        let t_enter = std::time::Instant::now();
        crate::dbg_trace!(
            "wait",
            "enter name={} pid={} group={:?} has_pump={} has_stderr_pump={}",
            self.name,
            pid,
            self.group_owner,
            self.pump.is_some(),
            self.stderr_pump.is_some(),
        );
        // Every external wait polls, because `wait_handling_stop` blocks in a
        // syscall that consults nothing: a blocking wait could be preempted
        // neither by a watchdog cancel (exarch's tool timeout on a `find`
        // chewing through node_modules) nor by a signal the platform handler
        // translated into a cause.
        //
        // `try_wait_handling_stop`, not `Child::try_wait`, so a SIGSTOP'd child
        // is seen (WUNTRACED): plain `try_wait` reports `Ok(None)` on a stop
        // and the loop would spin forever.  Every stop is `SIGCONT`ed here;
        // only a terminal outcome leaves the loop, to be classified below.
        let early_outcome: Option<crate::process::WaitOutcome> = {
            // Snappy for short-lived children, gentle on CPU for long ones.
            let mut interval = std::time::Duration::from_millis(5);
            let cap = std::time::Duration::from_millis(100);
            #[cfg(debug_assertions)]
            let mut polls: u32 = 0;
            loop {
                #[cfg(debug_assertions)]
                {
                    polls += 1;
                }
                match child.try_wait_handling_stop() {
                    Ok(Some(crate::process::WaitPoll::Stopped(sig))) => {
                        // The one rule: a stop is resumed at once by whoever
                        // waits on it, every role.
                        crate::dbg_trace!(
                            "wait",
                            "stopped-cont name={} pid={} polls={} elapsed={:?} signal={sig:?}",
                            self.name,
                            pid,
                            polls,
                            t_enter.elapsed(),
                        );
                        #[cfg(unix)]
                        crate::process::cont_stage_by_pid(pid);
                        continue;
                    }
                    Ok(Some(crate::process::WaitPoll::Done(o))) => {
                        crate::dbg_trace!(
                            "wait",
                            "exit-via-try_wait name={} pid={} polls={} elapsed={:?} outcome={:?}",
                            self.name,
                            pid,
                            polls,
                            t_enter.elapsed(),
                            o,
                        );
                        break Some(o);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::dbg_trace!(
                            "wait",
                            "try_wait err name={} pid={} polls={} elapsed={:?} err={e}",
                            self.name,
                            pid,
                            polls,
                            t_enter.elapsed(),
                        );
                        // Hand the child back to re-arm `Drop`; an unwaited
                        // child would otherwise leak with its pumps.
                        self.child = Some(child);
                        return Err(Break::Error(Error::new(format!("{}: {e}", self.name), 1)));
                    }
                }
                if let Some(cause) = self.cancel.cause() {
                    crate::dbg_trace!(
                        "wait",
                        "cancel-fired name={} pid={} polls={} elapsed={:?} cause={cause:?}",
                        self.name,
                        pid,
                        polls,
                        t_enter.elapsed(),
                    );
                    // A reaped leader carries its outcome straight out, so we
                    // never wait on a dead pid.  The Windows group release is
                    // not part of teardown; the `Standalone` branch below still
                    // performs it on the way out.
                    self.ending.raise(Ending::RalEnded(cause));
                    break self.terminate_group(&mut child, cause);
                }
                std::thread::sleep(interval);
                interval = (interval * 2).min(cap);
            }
        };
        let outcome = if let Some(o) = early_outcome {
            o
        } else {
            // Reached only when the poll broke via cancel without the grace
            // peek reaping: the group kill is already in flight, so this blocks
            // only as long as a dying child takes.  Traced on both sides so a
            // hang in waitpid / WaitForSingleObject is visible.  A stop seen
            // here is answered the same as the poll loop's own, inline.
            crate::dbg_trace!(
                "wait",
                "blocking-wait name={} pid={} elapsed={:?}",
                self.name,
                pid,
                t_enter.elapsed(),
            );
            let out = loop {
                match child.wait_handling_stop() {
                    Ok(crate::process::WaitPoll::Stopped(_)) => {
                        #[cfg(unix)]
                        crate::process::cont_stage_by_pid(pid);
                    }
                    Ok(crate::process::WaitPoll::Done(o)) => break o,
                    Err(e) => {
                        // Re-arm `Drop`, as in the poll loop's error arm.
                        self.child = Some(child);
                        return Err(Break::Error(Error::new(format!("{}: {e}", self.name), 1)));
                    }
                }
            };
            crate::dbg_trace!(
                "wait",
                "blocking-wait-done name={} pid={} elapsed={:?}",
                self.name,
                pid,
                t_enter.elapsed(),
            );
            out
        };
        // A death by a signal on our own ladder is our doing, so the report
        // names the cause rather than the number; anything else the child met in
        // the grace window stays its own and is reported as such.
        let outcome = match self.ending.get() {
            Ending::RalEnded(cause) => outcome.attribute_to(cause),
            Ending::OwnAccord => outcome,
        };
        // Let the Job Object's whole-job completion drain any descendants before
        // the handle goes.  A pipeline stage never lands here: its release
        // belongs to `PipelineGroup::Drop`.
        #[cfg(windows)]
        if let GroupOwner::Standalone(group) = self.group_owner {
            crate::dbg_trace!(
                "wait",
                "win-job-drain-begin name={} pid={} leader={} elapsed={:?}",
                self.name,
                pid,
                group,
                t_enter.elapsed(),
            );
            let _ = crate::process::wait_leader_blocking(group);
            crate::process::release_win_group(group.as_raw());
            crate::dbg_trace!(
                "wait",
                "win-job-drain-end name={} pid={} elapsed={:?}",
                self.name,
                pid,
                t_enter.elapsed(),
            );
        }
        // The leader is dead here — the `Stopped` branch already returned — so
        // kill the cgroup lest a straggler outlive the command, then remove it.
        // Windows releases its own group bookkeeping at this same point.
        #[cfg(target_os = "linux")]
        if let Some(jail) = &self.jail {
            crate::process::jail::linux::kill(jail);
            crate::process::jail::linux::remove(jail);
        }
        crate::dbg_trace!(
            "wait",
            "ready-for-drain name={} pid={} elapsed={:?} outcome={:?}",
            self.name,
            pid,
            t_enter.elapsed(),
            outcome,
        );
        Ok(WaitedChild {
            outcome,
            ending: self.ending.get(),
            pump: self.pump.take(),
            stderr_pump: self.stderr_pump.take(),
            name: self.name.clone(),
            pid,
            t_enter,
        })
    }
}

impl RunningChild {
    /// Run this external pipeline stage to its own end, on the caller's own
    /// dedicated waiter thread — meant to *be* that thread, spawned once per
    /// stage by `runtime::pipeline::launch`.  The sole owner of this child's
    /// wait from here on: nothing else may `waitpid` / `WaitForSingleObject`
    /// it once this starts, which is what makes pid reuse a non-issue.
    ///
    /// A stop is answered with `SIGCONT` inline, the same rule as
    /// [`Self::wait`]'s: the collector never hears of it, and nothing here
    /// tracks a stop across the loop.
    ///
    /// Never called for a standalone (non-pipeline) command, whose own
    /// [`Self::wait`] keeps polling for [`Self::cancel`] — this stage's own
    /// end instead arrives as a real signal (the reader-gone cascade, a
    /// background group's stop-then-kill, the group's own teardown kill),
    /// which the blocking wait sees structurally, no poll needed.
    ///
    /// `kill_cause` — not [`Self::cancel`] — is what attributes the ending:
    /// `cancel` is the *mooring's* scope, shared with every sibling stage
    /// (and beyond), so reading it here would let one stage's own kill
    /// misattribute a death the mooring never actually asked for; `kill_cause`
    /// is this stage's own, set only by the collector's own `kill_now`/
    /// `cancel`, one waiter's business alone.
    pub(crate) fn run_pipeline_stage(
        mut self,
        kill_cause: &crate::process::CancelScope,
    ) -> (String, Settled<Option<crate::process::CommandFailure>>) {
        let mut child = self.child.take().expect("RunningChild has no child");
        let name = self.name.clone();
        #[cfg(unix)]
        let pid = child.id();
        let terminal = loop {
            match child.wait_handling_stop() {
                Ok(crate::process::WaitPoll::Stopped(_)) => {
                    #[cfg(unix)]
                    crate::process::cont_stage_by_pid(pid);
                }
                Ok(crate::process::WaitPoll::Done(o)) => break o,
                Err(e) => {
                    let msg = format!("{name}: {e}");
                    return (name, Err(Break::Error(Error::new(msg, 1))));
                }
            }
        };
        let cause = kill_cause.cause();
        let outcome = cause.map_or(terminal, |c| terminal.attribute_to(c));
        let ending = cause.map_or(Ending::OwnAccord, Ending::RalEnded);
        #[cfg(target_os = "linux")]
        if let Some(jail) = &self.jail {
            crate::process::jail::linux::kill(jail);
            crate::process::jail::linux::remove(jail);
        }
        let failure = crate::process::CommandFailure::from_outcome(outcome, ending);
        // A reader-gone kill's remaining bytes are owed to nobody, and a
        // descendant that survived that pid-addressed kill still holds the
        // pipe the pump reads, so joining it would never return — mirrors
        // `WaitedChild::settle`'s same rule for the standalone path.
        if ending == Ending::RalEnded(CancelCause::ReaderGone) {
            drop((self.pump.take(), self.stderr_pump.take()));
        } else {
            if let Some(jh) = self.pump.take() {
                let _ = jh.join();
            }
            if let Some(jh) = self.stderr_pump.take() {
                let _ = jh.join();
            }
        }
        (name, Ok(failure))
    }
}

impl WaitedChild {
    /// Join the drainer threads — or, for a child ral killed because its
    /// reader was gone, detach them.  Its remaining bytes are owed to nobody,
    /// and a descendant that survived that pid-addressed kill still holds the
    /// pipe the pump reads, so the join would never return.  Every other
    /// ending still joins: a group teardown kills the whole tree before
    /// anything is observed (`PipelineGroup::kill`), and a standalone child's
    /// own teardown addresses its group, so in both the pumps are already at
    /// EOF and the capture is complete.
    pub fn settle(mut self) {
        if self.ending == Ending::RalEnded(CancelCause::ReaderGone) {
            drop((self.pump.take(), self.stderr_pump.take()));
            return;
        }
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
            crate::dbg_trace!(
                "wait",
                "drain-stdout-joined name={} pid={} elapsed={:?}",
                self.name,
                self.pid,
                self.t_enter.elapsed(),
            );
        }
        if let Some(jh) = self.stderr_pump.take() {
            let _ = jh.join();
            crate::dbg_trace!(
                "wait",
                "drain-stderr-joined name={} pid={} elapsed={:?}",
                self.name,
                self.pid,
                self.t_enter.elapsed(),
            );
        }
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
    /// `wait` has taken the child — that is the success path's disarm.  A
    /// Windows `Standalone` releases its `win_groups` entry inline here, the
    /// job `wait` does after `wait_leader_blocking`; the kill is idempotent, so
    /// a `BorrowedByPipeline` stage racing the group owner's `Drop` is safe.
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.kill_group(&mut child);
        #[cfg(windows)]
        if let GroupOwner::Standalone(group) = self.group_owner {
            crate::process::release_win_group(group.as_raw());
        }
        #[cfg(target_os = "linux")]
        if let Some(jail) = &self.jail {
            crate::process::jail::linux::remove(jail);
        }
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }
        if let Some(jh) = self.stderr_pump.take() {
            let _ = jh.join();
        }
        let _ = child.reap();
    }
}

#[cfg(unix)]
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::process::*;

    /// A wall that expires mid-command must be reported as the time limit it
    /// was, not as the SIGTERM we happened to send — and reported without
    /// disturbing the status, which stays the signal's 128 + 15.
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
            ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            GroupOwner::Standalone(pgid.expect("NewLeader yields a tracked pgid")),
            scope.clone(),
            None,
        );
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Deadline);
        });

        let waited = running.wait().expect("wait should not error");
        let failure = crate::process::CommandFailure::from_outcome(waited.outcome, waited.ending)
            .expect("a torn-down child is a failure");
        waited.settle();
        canceller.join().expect("canceller thread");

        assert_eq!(
            failure.to_user_exit_code(),
            128 + libc::SIGTERM,
            "the status must not shift"
        );
        assert_eq!(
            failure.message("sleep"),
            "sleep: stopped because the call's time limit expired"
        );
    }

    /// An interrupt opens with the gentler SIGINT, to keep job-control
    /// semantics, and must still bound the whole tree.  Two properties, both
    /// asserted against a real subprocess: the 500 ms SIGINT grace in
    /// `terminate_group` must not become a wait on the child's own 30 s sleep,
    /// and the follow-up group SIGKILL must reap a grandchild that the SIGINT
    /// never reached.
    ///
    /// `/bin/sh -c 'sleep 30 & echo $!; wait'` gives both: `&` detaches the
    /// grandchild from the leader's signal handling, and the leader then blocks
    /// in `wait`, so the call stays open until the grandchild dies.  Spawning
    /// under `PgidPolicy::NewLeader` and assembling as `Standalone` is what
    /// makes the group real and ours to kill — teardown addresses a pgid only
    /// for a group the child owns.
    #[test]
    fn interrupt_tears_down_external_subprocess_tree() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped());

        let (mut child, pgid) =
            spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader).expect("spawn /bin/sh under new pgid");
        assert!(pgid.is_some(), "NewLeader yields a tracked pgid");

        // Take stdout before assembling: with no `stdout_pump` sink the
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
            ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            GroupOwner::Standalone(pgid.expect("NewLeader yields a tracked pgid")),
            scope.clone(),
            None,
        );

        // Fire once `wait` is inside its poll loop; the backoff starts at 5 ms,
        // so the cause is observed promptly after.
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Interrupt);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait().expect("wait should not error");
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

    /// A stop is answered with `SIGCONT` at once by whoever waits on it —
    /// the one rule, with no owner above it and nothing tracking the stop.
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
            ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            GroupOwner::Standalone(pgid),
            CancelScope::root(),
            None,
        );

        let t0 = std::time::Instant::now();
        let waited = running.wait().expect("wait should not error");
        let elapsed = t0.elapsed();
        waited.settle();

        assert!(
            elapsed.as_secs() < 5,
            "a SIGSTOP'd child must be revived rather than hung: took {elapsed:?}"
        );
    }
}
