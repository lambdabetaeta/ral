//! In-flight child handles: [`RunningChild`] after spawn, [`WaitedChild`] after
//! the wait, and [`ExternalPlumbing`], the pump plan the caller hands in.  The
//! running → waited → settled typestate makes "settle before wait" and "wait
//! twice" unwritable.

use crate::io::Sink;
use crate::process::{CancelCause, Ending, KillTarget, StopPolicy};
#[cfg(unix)]
use crate::types::Escape;
use crate::types::{Break, Error, Settled};

/// Who releases the Windows group registered in `win_groups`
/// (`process::signal::windows`), which unlike a Unix pgid does not vanish when
/// its members exit.  `Standalone` releases its own once `wait` returns;
/// `BorrowedByPipeline` leaves it to `PipelineGroup` in `runtime::pipeline`, so
/// a `RunningChild::Drop` racing that owner's `Drop` cannot double-close;
/// `None` is a child spawned with `PgidPolicy::Inherit` and has no group at
/// all.  The pgid rides along with the variant that has one, so
/// `(pgid: Some, GroupOwner::None)` is unrepresentable.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GroupOwner {
    None,
    Standalone(crate::process::Pgid),
    #[cfg_attr(windows, allow(dead_code))]
    BorrowedByPipeline(crate::process::Pgid),
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
    /// What a stop `wait` itself sees means: `Escape` surfaces
    /// `Escape::Stopped` so the REPL can register the pgid as a job; `Park`
    /// waits on a pipeline's gate; `KillAndReap` kills and reaps on the spot.
    pub stop: StopPolicy,
    pub group_owner: GroupOwner,
    /// Polled by `wait`, because a blocking `waitpid` / `WaitForSingleObject`
    /// consults nothing: without the poll an upstream cancel (exarch's tool
    /// timeout, a signal the platform handler translated into a cause) could
    /// not preempt a child that never exits on its own.
    pub cancel: crate::process::CancelScope,
    /// An outcome a collector probe already collected, consumed by `wait`.
    settled: Option<crate::process::WaitOutcome>,
    /// How this child's life ended, once ral itself ended it: the cancel
    /// branch of `wait` and the pipeline collector's reader-gone kill are the
    /// only writers, and they join by `max`.  Sole input to forgiveness and
    /// to whether the drainers are joined.
    ending: Ending,
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
        stop: StopPolicy,
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
            stop,
            group_owner,
            cancel,
            settled: None,
            ending: Ending::OwnAccord,
        }
    }

    /// Who a signal this child sends addresses: its whole group, iff it owns
    /// one outright, else its own pid alone.  A stage borrowing a pipeline's
    /// group (`BorrowedByPipeline`) may never address that group — only the
    /// owning `PipelineGroup` may.
    fn kill_target(&self) -> KillTarget {
        match self.group_owner {
            GroupOwner::Standalone(p) => KillTarget::Group(p),
            GroupOwner::BorrowedByPipeline(_) | GroupOwner::None => KillTarget::Pid,
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

    /// Poll for the leader until `deadline`; `None` on timeout.
    #[cfg(unix)]
    fn grace_poll(
        child: &mut crate::process::ChildHandle,
        deadline: std::time::Instant,
    ) -> Option<crate::process::WaitOutcome> {
        while std::time::Instant::now() < deadline {
            match child.try_wait_handling_stop() {
                Ok(Some(o)) => return Some(o),
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
    /// Under `StopPolicy::Escape` a stop short-circuits to `Escape::Stopped`:
    /// the process stays alive in stopped state with the pumps still on its
    /// pipes, for the REPL to register as a job.  Under `StopPolicy::Park` a
    /// stop instead records itself on the park's status and blocks on its
    /// gate, then resumes waiting on the same child — this method never
    /// returns for that case until the child truly ends.  `Drop`'s kill is
    /// already disarmed by then.
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
        // and the loop would spin forever.  A `Park` stop waits at the gate
        // here; every other outcome leaves the loop and is classified below.
        let early_outcome: Option<crate::process::WaitOutcome> = if let Some(o) =
            self.settled.take()
        {
            // A collector probe already took this child's event — an exit is
            // consumed, a stop leaves it alive but noted — so the poll must
            // not run `try_wait_handling_stop` again.
            Some(o)
        } else {
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
                    Ok(Some(crate::process::WaitOutcome::Stopped(sig)))
                        if let StopPolicy::Park(park) = &self.stop =>
                    {
                        park.stop.set(Some(sig));
                        crate::dbg_trace!(
                            "wait",
                            "parked name={} pid={} polls={} elapsed={:?} signal={sig:?}",
                            self.name,
                            pid,
                            polls,
                            t_enter.elapsed(),
                        );
                        match park.gate.wait(&self.cancel) {
                            // The gate opened: the child is alive again
                            // (`SIGCONT`ed), so keep waiting on the same one.
                            Ok(()) => continue,
                            Err(cause) => {
                                self.ending = self.ending.max(Ending::RalEnded(cause));
                                break self.terminate_group(&mut child, cause);
                            }
                        }
                    }
                    Ok(Some(o)) => {
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
                    self.ending = self.ending.max(Ending::RalEnded(cause));
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
            // hang in waitpid / WaitForSingleObject is visible.
            crate::dbg_trace!(
                "wait",
                "blocking-wait name={} pid={} elapsed={:?}",
                self.name,
                pid,
                t_enter.elapsed(),
            );
            let out = match child.wait_handling_stop() {
                Ok(out) => out,
                Err(e) => {
                    // Re-arm `Drop`, as in the poll loop's error arm.
                    self.child = Some(child);
                    return Err(Break::Error(Error::new(format!("{}: {e}", self.name), 1)));
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
        let outcome = match self.ending {
            Ending::RalEnded(cause) => outcome.attribute_to(cause),
            Ending::OwnAccord => outcome,
        };
        // A stop is job control's business only while nothing is ending this
        // child; once ral is, it is a corpse to be.  `Park` never arrives
        // here: the poll loop above loops back onto the same child.
        #[cfg(unix)]
        let outcome = match outcome {
            crate::process::WaitOutcome::Stopped(signal) => {
                match (&self.stop, self.ending, self.group_owner) {
                    // A group is what a job is keyed by, so a child without one
                    // cannot become one however its policy reads.
                    (
                        StopPolicy::Escape,
                        Ending::OwnAccord,
                        GroupOwner::Standalone(pgid) | GroupOwner::BorrowedByPipeline(pgid),
                    ) => {
                        // Detach the pumps rather than join them: the stopped
                        // child still holds its pipes open.
                        drop((self.pump.take(), self.stderr_pump.take()));
                        return Err(Break::Escape(Escape::Stopped {
                            pgid,
                            signal,
                            cmd: self.name.clone(),
                            // Empty at birth: the frames that staged writes
                            // hang them on the escape as it passes them.
                            pending: Vec::new(),
                        }));
                    }
                    _ => match child.kill_and_reap_stopped(signal, self.kill_target()) {
                        Ok(out) => out,
                        Err(e) => {
                            self.child = Some(child);
                            return Err(Break::Error(Error::new(format!("{}: {e}", self.name), 1)));
                        }
                    },
                }
            }
            other => other,
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
            ending: self.ending,
            pump: self.pump.take(),
            stderr_pump: self.stderr_pump.take(),
            name: self.name.clone(),
            pid,
            t_enter,
        })
    }
}

impl RunningChild {
    /// Wait, classify, and settle the drainers: the failure the outcome
    /// amounts to, or `None` for success.  The raw status does not come back
    /// beside the verdict, so no caller can branch on a forgiven death; where
    /// a status still means something it is `failure.to_user_exit_code()`,
    /// reachable only inside `Some`.
    pub(crate) fn observe(self) -> Settled<Option<crate::process::CommandFailure>> {
        let waited = self.wait()?;
        let failure = crate::process::CommandFailure::from_outcome(waited.outcome, waited.ending);
        waited.settle();
        Ok(failure)
    }

    /// One non-blocking probe: note the child's end or stop if it has one,
    /// caching the outcome for the eventual `wait`.  Returns whether this
    /// child is ready to observe.
    pub(crate) fn try_settle(&mut self) -> bool {
        if self.settled.is_some() {
            return true;
        }
        let Some(child) = self.child.as_mut() else {
            return true;
        };
        match child.try_wait_handling_stop() {
            Ok(Some(outcome)) => {
                self.settled = Some(outcome);
                true
            }
            Ok(None) => false,
            // `wait` retries and surfaces the same error with its context.
            Err(_) => true,
        }
    }

    /// The stop signal `try_settle` remembered, if the settled outcome was
    /// one; `None` otherwise, including "not yet settled".
    pub(crate) fn remembered_stop(&self) -> Option<crate::process::Signal> {
        match self.settled {
            Some(crate::process::WaitOutcome::Stopped(sig)) => Some(sig),
            _ => None,
        }
    }

    /// Forget a remembered stop on resume, so the next `try_settle` waits on
    /// this child fresh rather than replaying the stop it already reported.
    pub(crate) fn clear_remembered_stop(&mut self) {
        if matches!(self.settled, Some(crate::process::WaitOutcome::Stopped(_))) {
            self.settled = None;
        }
    }

    /// The pipeline collector's own kill, for a stage whose reader is reaped.
    /// It addresses the pid alone — the anchor and unrelated group members
    /// still live — and lands harmlessly on an already-exited child, which is
    /// what keeps a recorded exit status from ever being overwritten.
    pub(crate) fn reader_gone(&mut self) {
        self.ending = self.ending.max(Ending::RalEnded(CancelCause::ReaderGone));
        let Some(child) = self.child.as_mut() else {
            return;
        };
        #[cfg(unix)]
        {
            let _ = child.kill();
        }
        #[cfg(windows)]
        {
            crate::process::signal::terminate_for_stage_kill(child.raw_process_handle());
        }
    }

    /// Kill and reap a stopped child whose stop the collector answered with
    /// death, recording the stop as its verdict: `waitpid` reports a stop
    /// once, so a later wait would find nothing and the report would lose it.
    #[cfg(unix)]
    pub(crate) fn end_stopped(&mut self, sig: crate::process::Signal) {
        let target = self.kill_target();
        if let Some(child) = self.child.as_mut()
            && let Ok(outcome) = child.kill_and_reap_stopped(sig, target)
        {
            self.settled = Some(outcome);
        }
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
            StopPolicy::KillAndReap,
            GroupOwner::Standalone(pgid.expect("NewLeader yields a tracked pgid")),
            scope.clone(),
            None,
        );
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Deadline);
        });

        let failure = running
            .observe()
            .expect("wait should not error")
            .expect("a torn-down child is a failure");
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
            // No parking: a stop here would be killed and reaped, not
            // turned into a job.
            StopPolicy::KillAndReap,
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
}
