//! In-flight child handles: [`RunningChild`] after spawn, [`WaitedChild`] after
//! the wait, and [`ExternalPlumbing`], the pump plan the caller hands in.  The
//! running → waited → drained typestate makes "drain before wait" and "wait
//! twice" unwritable.

use crate::io::Sink;
#[cfg(unix)]
use crate::types::Escape;
use crate::types::{Break, Error, Settled};

/// Who releases the Windows group registered in `win_groups`
/// (`process::signal::windows`), which unlike a Unix pgid does not vanish when
/// its members exit.  `Standalone` releases its own once `wait` returns;
/// `BorrowedByPipeline` leaves it to `PipelineGroup` in `runtime::pipeline`, so
/// a `RunningChild::Drop` racing that owner's `Drop` cannot double-close;
/// `None` is a child spawned with `PgidPolicy::Inherit` and has no group at all.
/// Inert on Unix, but kept so the cross-platform shape stays uniform.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GroupOwner {
    None,
    #[cfg_attr(unix, allow(dead_code))]
    Standalone,
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
/// descendants — `/bin/sh -c 'sleep 999'` leaves no orphan behind.
///
/// Audit-agnostic: byte capture belongs to the caller.  A standalone external
/// is teed at dispatch level by `evaluator::with_audit_capture`; a direct-spawn
/// pipeline stage writes into the next stage's pipe and gets a synthesised node
/// with empty stdout from `runtime::pipeline::collect`.
pub(crate) struct RunningChild {
    pub child: Option<crate::process::ChildHandle>,
    pub pgid: Option<crate::process::Pgid>,
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
    /// On `WIFSTOPPED`, surface `Escape::Stopped` so the REPL can register the
    /// pgid as a job; `false` kills and reaps instead.  Set for a foreground
    /// child of an interactive REPL, and for a pipeline that owns the tty.
    pub park_on_stop: bool,
    #[allow(dead_code)]
    pub group_owner: GroupOwner,
    /// Polled by `wait`, because a blocking `waitpid` / `WaitForSingleObject`
    /// consults nothing: without the poll an upstream cancel (exarch's tool
    /// timeout, a signal the platform handler translated into a cause) could
    /// not preempt a child that never exits on its own.
    pub cancel: crate::process::CancelScope,
}

/// A child observed dead, holding its outcome and its not-yet-joined drainers.
/// [`RunningChild::wait`] is the only constructor, so atomic-redirect commit and
/// status interpretation carry a borrow-check proof that the child has exited.
pub(crate) struct WaitedChild {
    pub outcome: crate::process::WaitOutcome,
    pump: Option<std::thread::JoinHandle<()>>,
    stderr_pump: Option<std::thread::JoinHandle<()>>,
    /// Trace context carried from the `RunningChild` so `drain`'s pump-join
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
        pgid: Option<crate::process::Pgid>,
        name: String,
        plumbing: ExternalPlumbing,
        park_on_stop: bool,
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
            pgid,
            jail,
            pump,
            stderr_pump,
            name,
            park_on_stop,
            group_owner,
            cancel,
        }
    }

    /// SIGKILL the process group, or the child alone when none is tracked.
    /// Idempotent on both platforms, so [`Self::wait`]'s cancel branch and
    /// [`Drop`] may each call it without coordinating.  It does not release the
    /// Windows group bookkeeping: `wait` does that after `wait_leader_blocking`,
    /// `Drop` inline.
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
        match self.pgid {
            Some(group) => {
                let _ = rustix::process::kill_process_group(
                    group.as_pid(),
                    rustix::process::Signal::KILL,
                );
            }
            None => {
                let _ = child.kill();
            }
        }
        #[cfg(windows)]
        match (self.group_owner, self.pgid) {
            (GroupOwner::Standalone | GroupOwner::BorrowedByPipeline, Some(p)) => {
                crate::process::kill_pipeline_group(p);
            }
            _ => {
                let _ = child.kill();
            }
        }
        #[cfg(not(any(unix, windows)))]
        let _ = child.kill();
    }

    /// Poll for the leader until `deadline`, classifying a stop as `wait` does.
    /// `None` on timeout.
    #[cfg(unix)]
    fn grace_poll(
        &self,
        child: &mut crate::process::ChildHandle,
        deadline: std::time::Instant,
    ) -> Option<crate::process::WaitOutcome> {
        while std::time::Instant::now() < deadline {
            match child.try_wait_handling_stop(self.pgid, self.park_on_stop) {
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
    /// The grace signal stays pgid-addressed even under a jail — a grandchild
    /// that `setsid()`'d away would miss it either way — but the final kill goes
    /// through [`Self::kill_group`], where `cgroup.kill` does catch it.
    fn terminate_group(
        &self,
        child: &mut crate::process::ChildHandle,
        cause: crate::process::CancelCause,
    ) -> Option<crate::process::WaitOutcome> {
        #[cfg(unix)]
        {
            let Some(pgid) = self.pgid else {
                // An `Inherit`-placed child gets the same escalation ladder,
                // addressed to its pid: dropping straight to SIGKILL would deny
                // this one child the grace every other path grants.
                if cause == crate::process::CancelCause::RootAbort {
                    self.kill_group(child);
                    return None;
                }
                let signal = match cause {
                    crate::process::CancelCause::Interrupt => rustix::process::Signal::INT,
                    _ => rustix::process::Signal::TERM,
                };
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps"
                )]
                let _ = rustix::process::kill_process(
                    rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
                    signal,
                );
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                let reaped = self.grace_poll(child, deadline);
                if reaped.is_none() {
                    self.kill_group(child);
                }
                return reaped;
            };
            if cause == crate::process::CancelCause::RootAbort {
                self.kill_group(child);
                return None;
            }
            let signal = match cause {
                crate::process::CancelCause::Interrupt => libc::SIGINT,
                _ => libc::SIGTERM,
            };
            pgid.signal_group(crate::process::Signal::new(signal));
            // Short — a timed-out call is already over budget — but enough for
            // a test runner to print its summary and exit.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            let reaped = self.grace_poll(child, deadline);
            // Harmless on a tree that already left, decisive against a
            // grandchild that trapped the signal and still holds the pipe.
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
    /// A stop on a tracked pgid short-circuits to `Escape::Stopped`: the process
    /// stays alive in stopped state with the pumps still on its pipes, for the
    /// REPL to register as a job.  `Drop`'s kill is already disarmed by then.
    pub fn wait(mut self) -> Settled<WaitedChild> {
        // Taking the child disarms `Drop` for the success path.
        let mut child = self.child.take().expect("RunningChild has no child");
        let pid = child.id();
        let t_enter = std::time::Instant::now();
        crate::dbg_trace!(
            "wait",
            "enter name={} pid={} pgid={:?} group={:?} park_on_stop={} has_pump={} has_stderr_pump={}",
            self.name,
            pid,
            self.pgid,
            self.group_owner,
            self.park_on_stop,
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
        // is seen (WUNTRACED) and classified — parked when `park_on_stop`,
        // killed and reaped otherwise; plain `try_wait` reports `Ok(None)` on a
        // stop and the loop would spin forever.  `Some(outcome)` therefore means
        // the child is already consumed, and must not be waited on again.
        let early_outcome: Option<crate::process::WaitOutcome> = {
            // Snappy for short-lived children, gentle on CPU for long ones.
            let mut interval = std::time::Duration::from_millis(5);
            let cap = std::time::Duration::from_millis(100);
            let mut polls: u32 = 0;
            loop {
                polls += 1;
                match child.try_wait_handling_stop(self.pgid, self.park_on_stop) {
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
                "blocking-wait name={} pid={} park_on_stop={} elapsed={:?}",
                self.name,
                pid,
                self.park_on_stop,
                t_enter.elapsed(),
            );
            let out = match child.wait_handling_stop(self.pgid, self.park_on_stop) {
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
        #[cfg(unix)]
        if let crate::process::WaitOutcome::Stopped(signal) = outcome {
            let pgid = self
                .pgid
                .expect("wait_handling_stop only returns Stopped when pgid is Some");
            // Detach the pumps rather than join them: the stopped child still
            // holds its pipes open, so a join here would never return.
            let _ = self.pump.take();
            let _ = self.stderr_pump.take();
            return Err(Break::Escape(Escape::Stopped {
                pgid,
                signal,
                cmd: self.name.clone(),
            }));
        }
        // Let the Job Object's whole-job completion drain any descendants before
        // the handle goes.  A pipeline stage never lands here: its release
        // belongs to `PipelineGroup::Drop`.
        #[cfg(windows)]
        if matches!(self.group_owner, GroupOwner::Standalone)
            && let Some(group) = self.pgid
        {
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
            pump: self.pump.take(),
            stderr_pump: self.stderr_pump.take(),
            name: self.name.clone(),
            pid,
            t_enter,
        })
    }
}

impl RunningChild {
    /// Wait, classify, and join the drainers: the user-visible exit code paired
    /// with whatever failure the outcome maps to.  `is_pipeline_non_final`
    /// forgives a SIGPIPE as success.  The pipeline collector and the helper
    /// stage in `runtime::pipeline` both reduce a child this way, so it lives
    /// here once.
    pub(crate) fn observe(
        self,
        is_pipeline_non_final: bool,
    ) -> Settled<(i32, Option<crate::process::CommandFailure>)> {
        let waited = self.wait()?;
        let outcome = waited.outcome;
        let code = outcome.to_user_exit_code();
        let failure = crate::process::CommandFailure::from_outcome(outcome, is_pipeline_non_final);
        waited.drain();
        Ok((code, failure))
    }

    /// Release the OS child without killing or reaping it, for when a sibling
    /// stage has parked the pipeline pgid: the kernel holds every stage stopped,
    /// and the pumps exit when a later `fg` runs the pipeline out.  Disarms
    /// `Drop`, group release included — that stays with `PipelineGroup`.
    #[cfg(unix)]
    pub(crate) fn abandon(mut self) {
        let _ = self.child.take();
        let _ = self.pump.take();
        let _ = self.stderr_pump.take();
    }
}

impl WaitedChild {
    /// Join the drainer threads.  Consumes `self`, since they join exactly
    /// once; a `WaitedChild` only exists past the child's death, so the joins
    /// meet a pipe already at EOF.
    pub fn drain(mut self) {
        crate::dbg_trace!(
            "wait",
            "drain-begin name={} pid={} elapsed={:?} has_pump={} has_stderr_pump={}",
            self.name,
            self.pid,
            self.t_enter.elapsed(),
            self.pump.is_some(),
            self.stderr_pump.is_some(),
        );
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
        if let (GroupOwner::Standalone, Some(group)) = (self.group_owner, self.pgid) {
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
    /// under `PgidPolicy::NewLeader` is what makes the group real — teardown
    /// addresses a pgid, not a pid.
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
            pgid,
            "sh".to_string(),
            ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            // No parking: a stop here would be killed and reaped, not
            // turned into a job.
            false,
            GroupOwner::None,
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
        waited.drain();
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
