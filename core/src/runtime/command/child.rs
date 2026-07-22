//! In-flight child handles: [`RunningChild`] (after spawn), [`WaitedChild`]
//! (after `wait_handling_stop`), and [`ExternalPlumbing`] (the per-call
//! pump plan handed in by the caller).  The typestate from running → waited
//! → drained makes "drain before wait" and "wait twice" unwritable.

use crate::io::Sink;
#[cfg(unix)]
use crate::types::Escape;
use crate::types::{Break, Error, Settled};

/// Who owns the platform process-group bookkeeping on Windows.
///
/// On Unix the kernel manages pgid lifetime: a member exiting is enough
/// to update the pgroup, and there's nothing to release.  On Windows
/// every "leader" registers a `GroupState` in `signal::win_groups` that
/// must be explicitly released; the question is *who* releases it.
///
///   * `Standalone`: this `RunningChild` is the only owner.  Aborting
///     calls `TerminateJobObject` on the group; success calls
///     `release_win_group` once `wait` returns.
///   * `BorrowedByPipeline`: a [`super::super::pipeline::group::PipelineGroup`]
///     owns the group; this child only borrows membership and must not
///     release the group on its own (a `RunningChild::Drop` racing
///     `PipelineGroup::Drop` would otherwise double-close).
///   * `None`: no group bookkeeping (child spawned with
///     `PgidPolicy::Inherit`).
///
/// The variants compile to no code on Unix; the field still exists so
/// the cross-platform shape stays uniform.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GroupOwner {
    None,
    #[cfg_attr(unix, allow(dead_code))]
    Standalone,
    BorrowedByPipeline,
}

/// A spawned external child plus the threads draining its piped
/// stdout/stderr.  The shared core of standalone exec and pipeline
/// external stages.
///
/// Lifecycle is a small typestate:
///
/// ```text
///     RunningChild ──wait──► WaitedChild ──drain──► ()
///         │  Drop                │  Drop
///         ▼                      ▼
///     SIGKILL+reap           join+drop
/// ```
///
/// `wait` consumes `RunningChild` and yields a `WaitedChild` that
/// carries the `WaitOutcome`.  `drain` can only be invoked on `WaitedChild`,
/// so the bug class "join pumps before wait" (which would let drainers
/// block on a still-open pipe) is unwritable: there's no
/// `RunningChild::drain`.
/// Likewise "wait twice" is unwritable: `wait` consumes self, so a
/// second call has nothing to consume.
///
/// Holding the pgid (not just the pid) means abort-path SIGKILL takes
/// out descendants too — `/bin/sh -c 'sleep 999'` doesn't leave the
/// `sleep` alive when its parent goes away.  The Option around `child`
/// is the disarm latch for `Drop`: `wait` takes the child to call
/// `wait_handling_stop` and never puts it back, so the abort path's
/// kill/reap logic short-circuits if `wait` already ran.
///
/// Audit-agnostic.  Per-command byte capture is the *caller's* concern:
/// pipeline stages own their `tee_with_buffer` arcs on `ProcessHandle`;
/// standalone externals are captured at dispatch level by
/// `evaluator::with_audit_capture` (which tees `shell.turn.io.stdout/stderr`).
/// `RunningChild` carries no audit fields.
pub(crate) struct RunningChild {
    pub child: Option<crate::process::ChildHandle>,
    pub pgid: Option<crate::process::Pgid>,
    /// The transient guest-jail cgroup this child was placed in, if any —
    /// `None` on every platform but a real Linux guest.  `kill_group` and
    /// `wait`/`Drop`'s cleanup prefer this over the pgid once it is `Some`:
    /// a grandchild that `setsid()`'d away escapes `kill(-pgid, …)` but
    /// cannot leave its cgroup.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub jail: Option<crate::process::jail::JailCgroup>,
    pub pump: Option<std::thread::JoinHandle<()>>,
    /// Pump thread draining piped stderr into the caller-supplied sink.
    /// `None` when stderr was inherited / file / `2>&1`.
    pub stderr_pump: Option<std::thread::JoinHandle<()>>,
    /// Display name used in wait-error messages.
    pub name: String,
    /// On `WIFSTOPPED`, surface `Escape::Stopped` so the REPL can
    /// register the pgid as a job.  `false` falls back to the
    /// kill-and-reap path: standalone foreground externals opt in;
    /// pipeline stages opt out (until the anchor process can be parked
    /// alongside).
    pub park_on_stop: bool,
    /// Windows: who owns the platform group registered for `pgid`.
    /// Unused on Unix (kernel manages pgroup lifetime).
    #[allow(dead_code)]
    pub group_owner: GroupOwner,
    /// Cooperative cancel handle.  Polled in `wait` so an upstream
    /// scope cancel — exarch's tool-timeout watchdog, or a signal the
    /// platform handler translated into a cause — actually preempts a
    /// blocked child: a blocking `wait_handling_stop` would otherwise
    /// sit in `waitpid` / `WaitForSingleObject` until the child exited
    /// on its own, no matter how long ago the flag was set.
    pub cancel: crate::process::CancelScope,
}

/// A `RunningChild` whose `wait_handling_stop` returned successfully.
/// Holds the `WaitOutcome` plus the still-running drainer threads (which
/// see EOF as soon as the child's write ends close at termination, but
/// haven't necessarily been joined yet).
///
/// Construction is gated by [`RunningChild::wait`]; there is no other
/// path.  Atomic-redirect commit and status interpretation therefore
/// have a borrow-check proof of "child has been observed dead" before
/// they run.
pub(crate) struct WaitedChild {
    pub outcome: crate::process::WaitOutcome,
    pump: Option<std::thread::JoinHandle<()>>,
    stderr_pump: Option<std::thread::JoinHandle<()>>,
    /// Trace context carried over from the originating `RunningChild`
    /// so `drain`'s pump-join timings can be attributed to the same
    /// command instance.  Debug-only; release builds compile the
    /// `dbg_trace` calls away but the fields are cheap enough to keep
    /// unconditional.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    name: String,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pid: u32,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    t_enter: std::time::Instant,
}

/// Post-spawn plumbing for an external child: where to pump stdout / stderr
/// when piped.  Both fields are `None` when the corresponding fd was
/// inherited or routed through a direct OS pipe (next pipeline stage's
/// stdin).  Both standalone exec and pipeline external stages compute one
/// of these and hand it to [`RunningChild::assemble_with_owner`], so
/// the `Drop` / wait rules of `RunningChild` cannot be re-derived at
/// each call site.
pub(crate) struct ExternalPlumbing {
    pub stdout_pump: Option<Sink>,
    pub stderr_pump: Option<Sink>,
}

impl RunningChild {
    /// Assemble a `RunningChild` from a freshly-spawned child plus a
    /// per-call plumbing plan.  Single source of truth for the
    /// stderr-reader / stderr-pump / stdout-pump / audit-tee triple, used
    /// by both [`super::run`] and the pipeline external stage launcher.
    /// `group_owner` declares who owns the
    /// platform group registered for `pgid`: standalone foreground
    /// externals on Windows pass `Standalone` so the abort path uses
    /// `TerminateJobObject` and the success path releases the group;
    /// pipeline stages pass `BorrowedByPipeline`; the
    /// [`super::super::pipeline::group::PipelineGroup`] owns the
    /// release.  Children with no group at all (Unix `Inherit`,
    /// builtins-via-spawn) pass `None`.
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

    /// SIGKILL the process group, falling back to a direct child kill
    /// when no group is tracked.  Idempotent: on Windows the underlying
    /// `TerminateJobObject` is safe against an already-terminated group;
    /// on Unix re-sending SIGKILL to a dead pgid is a no-op.  Shared by
    /// the cancel branch in [`Self::wait`] (preempt mid-flight, then
    /// fall through to `wait_handling_stop`) and by [`Drop`] (abort-path
    /// cleanup before reaping).
    ///
    /// Note: does *not* release the Windows group bookkeeping for
    /// `GroupOwner::Standalone`.  On the success path `wait` calls
    /// `release_win_group` after `wait_leader_blocking`; on the abort
    /// path `Drop` makes the equivalent call inline.
    ///
    /// On a Linux guest with a jail cgroup tracked, `cgroup.kill` is used
    /// instead of the pgid signal: it reaches a grandchild that
    /// `setsid()`'d out of the tracked pgid, which `kill(-pgid, …)`
    /// cannot — moving cgroups needs write permission on `cgroup.procs`,
    /// which the jail's unprivileged uid does not have.
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
            (GroupOwner::Standalone, Some(p)) | (GroupOwner::BorrowedByPipeline, Some(p)) => {
                crate::process::kill_pipeline_group(p);
            }
            _ => {
                let _ = child.kill();
            }
        }
        #[cfg(not(any(unix, windows)))]
        let _ = child.kill();
    }

    /// Poll for the leader to reap within `deadline`, handling a stop the
    /// same way `wait` does.  `Some(outcome)` if it exited (or the wait
    /// errored) inside the window, `None` on timeout.
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

    /// Cancel-path teardown: terminate the process group by the cancel
    /// cause, give the tree a short grace to exit on the signal it expects,
    /// then SIGKILL whatever is left.  Used when the cancel scope fires
    /// mid-wait (exarch's tool timeout, a Ctrl-C).  An interrupt goes
    /// SIGINT-first, an explicit cancel / deadline / termination request
    /// SIGTERM-first, and a root abort straight to SIGKILL with no grace.
    /// Signalling the *group* (not just the leader pid) takes out forked
    /// grandchildren too, closing the stdout pipe so the pump drain can
    /// complete.
    ///
    /// Returns `Some(outcome)` when the leader was reaped inside the grace
    /// peek (the caller must not `wait` again), `None` otherwise (the
    /// caller's follow-up blocking `wait_handling_stop` reaps it).  Falls
    /// back to a direct child kill when no group is tracked.
    ///
    /// The grace phase stays pgid-addressed: it delivers the signal a
    /// well-behaved tree expects, and a jailed grandchild that `setsid()`'d
    /// away would miss it either way. Every unconditional final kill goes
    /// through [`Self::kill_group`], so a tracked jail's `cgroup.kill`
    /// substitutes there.
    fn terminate_group(
        &self,
        child: &mut crate::process::ChildHandle,
        cause: crate::process::CancelCause,
    ) -> Option<crate::process::WaitOutcome> {
        #[cfg(unix)]
        {
            let Some(pgid) = self.pgid else {
                // No pgid tracked (an `Inherit`-placed child): the same
                // TERM/INT-first grace ladder applies regardless of whether
                // a group is tracked — degrading straight to SIGKILL here
                // would deny this lone child the escalation every other
                // path documents.
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
            // Poll for the leader to drain on the chosen signal; the
            // grace is short — a timed-out tool call is already over
            // budget — but long enough for a test runner to print its
            // summary and exit.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            let reaped = self.grace_poll(child, deadline);
            // Unconditional final kill: harmless on a tree that already
            // exited cleanly, decisive against a grandchild that trapped
            // the chosen signal and is still holding the pipe open.
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
    /// Wait for the child to terminate via `wait_handling_stop`.
    /// Consumes `self`: a second call has nothing to consume, and the
    /// returned `WaitedChild` is the only handle to the drainer threads
    /// from this point on.  On error the `RunningChild` is dropped and
    /// `Drop` runs the SIGKILL+reap+join sequence.
    ///
    /// A stop on a tracked pgid short-circuits to
    /// [`Escape::Stopped`]: the OS process stays alive in stopped
    /// state, the pump threads stay attached to its pipes, and the REPL
    /// catches the signal to register the pgid as a job.  The pgid-kill
    /// in `Drop` is disarmed because `self.child.take()` already ran.
    pub fn wait(mut self) -> Settled<WaitedChild> {
        // Take the child out so Drop's kill/reap sequence is disarmed
        // on the success path.  Construction always sets `Some(_)`.
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
        // Cancel-aware wait poll.  `wait_handling_stop` blocks in a
        // syscall that does not consult the cancel scope, so a blocking
        // wait could not be preempted — not by a watchdog-driven
        // `cancel()` (exarch's tool timeout on a `find` chewing through
        // node_modules), not by a signal the platform handler translated
        // into a cause (a batch SIGINT, a systemd-style SIGTERM).  Every
        // external wait therefore goes through this one poll loop.
        //
        // Uses `try_wait_handling_stop` rather than `Child::try_wait`
        // so a SIGSTOP'd child is observed (WUNTRACED) and classified:
        // parked as a resumable job when `park_on_stop` (the interactive
        // REPL's Ctrl-Z), killed and reaped inside the peek otherwise.
        // A plain `try_wait` returns `Ok(None)` on stop and the loop
        // would spin forever.  When the peek returns `Some(outcome)` the
        // child has already been fully consumed (reaped on the exit
        // branch, parked or killed-and-reaped on the stop branch), so we
        // do not call `wait_handling_stop` again.
        let early_outcome: Option<crate::process::WaitOutcome> = {
            // Exponential backoff: snappy for short-lived children,
            // gentle on CPU for long ones.
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
                        // Re-arm `Drop`'s kill/reap by handing the child
                        // back: an unwaited child whose handle just dropped
                        // would otherwise leak the process and its pumps.
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
                    // Teardown by cause: SIGINT-first for an interrupt,
                    // SIGTERM-first for an explicit cancel, a deadline,
                    // or a termination request, an immediate SIGKILL for
                    // a root abort — minus the Windows group-release,
                    // which the post-wait Standalone branch below still
                    // does once `wait_handling_stop` returns.  If the
                    // leader was reaped inside the grace peek, carry that
                    // outcome out directly so we don't `wait` a dead pid.
                    break self.terminate_group(&mut child, cause);
                }
                std::thread::sleep(interval);
                interval = (interval * 2).min(cap);
            }
        };
        let outcome = if let Some(o) = early_outcome {
            o
        } else {
            // Blocking fallthrough, reached only when the poll loop broke
            // via cancel and `terminate_group` did not reap the leader
            // inside its grace peek: the group kill is already in flight,
            // so this reaps a dying child promptly.  Trace before/after
            // so a hang in waitpid / WaitForSingleObject is visible.
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
                    // Re-arm `Drop`'s kill/reap by handing the child back;
                    // see the poll-loop error arm above.
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
            // Detach pump threads: they remain blocked on the child's
            // open pipes until the child eventually exits or is killed.
            let _ = self.pump.take();
            let _ = self.stderr_pump.take();
            return Err(Break::Escape(Escape::Stopped {
                pgid,
                signal,
                cmd: self.name.clone(),
            }));
        }
        // Standalone Windows group: drain any descendants via the Job
        // Object's whole-job completion before releasing the handle.
        // Pipeline stages don't take this path — `PipelineGroup::Drop`
        // owns the release for them.
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
        // The leader is confirmed dead (the `Stopped` branch above already
        // returned): `cgroup.kill` on settle — a straggler the leader left
        // behind must not outlive the command — then remove the transient
        // cgroup, the same structural point Windows releases its own group
        // bookkeeping.
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
    /// Wait for the child, classify its outcome, and join the drainers.
    /// Returns the user-visible exit code paired with the failure (if any)
    /// the outcome maps to — `is_pipeline_non_final` forgives a non-final
    /// stage's SIGPIPE as success.  An [`Escape::Stopped`] or a wait
    /// error propagates as the `Break` from [`Self::wait`].
    ///
    /// The wait → outcome → failure → drain sequence is lockstep: both the
    /// pipeline collector and the helper-stage observer reduce a child the
    /// same way, so it lives here once.
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

    /// Release ownership of the OS child without killing or reaping it.
    /// Used when a sibling stage has parked the pipeline pgid: the
    /// kernel keeps every stage alive in stopped state, the pump
    /// threads detach and exit when the eventual `fg` resumes the
    /// pipeline to completion.  Disarms `Drop`'s SIGKILL+reap path.
    #[cfg(unix)]
    pub(crate) fn abandon(mut self) {
        let _ = self.child.take();
        let _ = self.pump.take();
        let _ = self.stderr_pump.take();
        // Don't release the group: `abandon` is invoked when a sibling
        // stage parked the pipeline pgid.  The owner's Drop is what
        // releases.
    }
}

impl WaitedChild {
    /// Join the drainer threads.  Consumes `self`: drainers can be
    /// joined exactly once.  Safe by construction: a `WaitedChild`
    /// only exists once the child has been observed dead, so drainers
    /// see pipe-EOF promptly.
    ///
    /// Captured bytes (when any) live in caller-owned `tee_with_buffer`
    /// arcs; the caller drains them after this returns to ensure all
    /// pump writes have settled.
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
    /// Abort-path cleanup: SIGKILL the pgid (or fall back to the
    /// direct PID), join the drainers, reap.  No-op when `wait`
    /// already took the child — that's the success path's disarm.
    ///
    /// Windows `GroupOwner::Standalone` additionally releases its
    /// `win_groups` bookkeeping; on the success path this is done by
    /// `wait` after `wait_leader_blocking`, but here we never call
    /// `wait`, so the release happens inline.  `kill_pipeline_group`
    /// (and the Unix `kill(-pgid)`) is idempotent, so a `BorrowedByPipeline`
    /// stage racing the pipeline owner's `Drop` is safe.
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

    /// The `Interrupt` branch of [`RunningChild::wait`]'s cancel-aware
    /// poll loop tears down an external child's whole process group
    /// decisively, even though it begins with the gentler SIGINT.
    ///
    /// An [`CancelCause::Interrupt`] is delivered SIGINT-first to
    /// preserve job-control semantics: a well-behaved foreground tree
    /// reacts to SIGINT as it would to a terminal Ctrl-C.  That gentler
    /// first signal must not weaken the guarantee that teardown bounds
    /// the whole tree.  Two properties are at stake, and this test
    /// asserts both end-to-end against a real subprocess:
    ///
    ///   * The SIGINT grace does not deadlock.  [`terminate_group`] sends
    ///     SIGINT, then polls for the leader to drain over a bounded
    ///     500 ms window before escalating.  The `wait` call must return
    ///     within a few seconds rather than blocking until the child's
    ///     own 30 s sleep elapses.
    ///   * The follow-up group SIGKILL reaps a grandchild the SIGINT did
    ///     not.  The fixture forks a `sleep` grandchild that ignores the
    ///     foreground SIGINT (it is not in the terminal foreground group
    ///     and `&` detaches it from the leader's signal handling), so
    ///     only the unconditional group SIGKILL at the end of
    ///     [`terminate_group`] can reap it.  Its death is observable: the
    ///     `/bin/sh` leader blocks in `wait`, holding the call open until
    ///     the grandchild dies.
    ///
    /// The fixture mirrors `exarch`'s `timeout_kills_external_subprocess_tree`:
    /// `/bin/sh -c 'sleep 30 & echo $!; wait'` prints the grandchild's
    /// pid on stdout, then the leader blocks in `wait`.  Spawning under a
    /// fresh process group ([`PgidPolicy::NewLeader`]) makes the group
    /// teardown meaningful — the cancel path addresses the whole group by
    /// pgid, not just the leader by pid.
    #[test]
    fn interrupt_tears_down_external_subprocess_tree() {
        // The grandchild prints its pid so the test can prove it was
        // reaped, then sleeps far past any reasonable test duration; the
        // `/bin/sh` leader blocks in `wait`, holding the call open until
        // the grandchild exits unless the cancel tears the group down.
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped());

        let (mut child, pgid) =
            spawn_with_pgid(&mut cmd, PgidPolicy::NewLeader).expect("spawn /bin/sh under new pgid");
        assert!(pgid.is_some(), "NewLeader yields a tracked pgid");

        // Take stdout before assembling the `RunningChild`: with no
        // `stdout_pump` Sink the `ChildHandle` would otherwise keep
        // stdout attached.  Read the single line (the grandchild pid) on
        // a reader thread; the `echo $!` is flushed at startup, so the
        // read returns long before teardown.
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
            // park_on_stop = false: the cancel poll loop is only entered
            // on this branch.
            false,
            GroupOwner::None,
            scope.clone(),
            None,
        );

        // Fire the interrupt from another thread once `wait` has had time
        // to enter its poll loop (the backoff starts at 5 ms, so the
        // cause is observed promptly after this).
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Interrupt);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait().expect("wait should not error");
        let elapsed = t0.elapsed();
        waited.drain();
        canceller.join().expect("canceller thread");

        // Property 1: SIGINT-first does not deadlock.  SIGINT grace is
        // 500 ms then group SIGKILL, so real time is well under a second;
        // 10 s is a generous ceiling far below the 30 s sleep.
        assert!(
            elapsed.as_secs() < 10,
            "interrupt teardown must not block on the child's own sleep: returned after {elapsed:?}"
        );

        // Property 2: the forked grandchild — which ignored the SIGINT —
        // is reaped by the follow-up group SIGKILL.  `kill(pid, 0)`
        // returns ESRCH once it is gone; poll briefly to absorb the
        // window between the group SIGKILL and the kernel reaping it.
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
