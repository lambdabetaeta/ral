//! Signal handling and process-group placement.
//!
//! The platform handlers translate SIGINT / SIGTERM / SIGHUP into a
//! [`CancelCause`](crate::process::CancelCause) on the scopes of
//! [`cancel`](crate::process::cancel) — SIGINT on the foreground run,
//! SIGTERM / SIGHUP on the durable root — while a separate ladder counts
//! deliveries and forces `_exit` on the third.  The portable remainder,
//! [`PgidPolicy`] and the poll point [`check`], lives here; `unix` and
//! `windows` exist only on their own platform, so neither can be linked
//! from this page.
//!
//! The two are exhaustive, here and throughout ral-core: `os_pipe` is an
//! unconditional dependency and builds on Unix and Windows alone, so a
//! third-platform arm is a branch no compiler ever reaches — scaffolding
//! that can only rot.

use std::num::NonZeroI32;
use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(unix)]
use super::outcome::Signal;
use super::outcome::WaitOutcome;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    ForegroundGuard, PipelineRelay, install_handlers, interrupt_foreground_child, quit_handler,
    relay_handler, reset_child_signals, spawn_detached, spawn_with_pgid, spawn_with_pgid_after,
    term_handler, termios_snapshot, try_waitpgid_eintr, waitpgid_eintr,
};
#[cfg(unix)]
pub(crate) use unix::{cause_signal, signal_stage_by_pid};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ForegroundGuard, ReapStatus, apply_group_active_process_limit,
    break_pipeline_group, disown_pipeline_group, install_handlers, is_known_group,
    kill_pipeline_group, relay_interrupt, release_win_group, reset_child_signals,
    set_active_process_limit, try_reap_leader, wait_leader_blocking,
};
#[cfg(windows)]
pub(crate) use windows::{
    PreparedGroup, close_prepared_group, prepare_group, prepared_job, register_prepared_group,
};

// ── Child handle ───────────────────────────────────────────────────────────
//
// Bare `Child::wait` / `try_wait` omit `WUNTRACED`: a SIGSTOP'd child reads as
// "still running" and `wait` blocks forever.  `clippy.toml` disallows the raw
// pair outside `ChildHandle`; the few exceptions carry an `#[allow]`.

/// A spawned child, waitable only through the stop-aware
/// [`Self::wait_handling_stop`] / [`Self::try_wait_handling_stop`].
pub struct ChildHandle(ChildRepr);

enum ChildRepr {
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(crate::process::launch::RawChild),
}

impl ChildHandle {
    pub fn from_std(child: std::process::Child) -> Self {
        Self(ChildRepr::Std(child))
    }

    #[cfg(windows)]
    pub(crate) fn from_windows_raw(child: crate::process::launch::RawChild) -> Self {
        Self(ChildRepr::RawWindows(child))
    }

    pub fn id(&self) -> u32 {
        match &self.0 {
            ChildRepr::Std(child) => child.id(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.id(),
        }
    }

    /// # Errors
    /// Returns `Err` if the platform kill — SIGKILL on Unix,
    /// `TerminateProcess` on Windows — fails.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.kill(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.kill(),
        }
    }

    pub fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stdout(),
        }
    }

    pub fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stderr(),
        }
    }

    #[cfg(windows)]
    pub(crate) fn raw_process_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        match &self.0 {
            ChildRepr::Std(child) => {
                use std::os::windows::io::AsRawHandle;
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            }
            ChildRepr::RawWindows(child) => child.raw_process_handle(),
        }
    }

    /// Blocking wait that returns on a stop too, as [`WaitOutcome::Stopped`];
    /// what the stop means is the holder's decision, never the wait's.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    pub(crate) fn wait_handling_stop(&mut self) -> std::io::Result<WaitOutcome> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::wait_handling_stop(child)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::wait_handling_stop(child),
                ChildRepr::RawWindows(child) => child.wait_handling_stop(),
            }
        }
    }

    /// Non-blocking peer of [`Self::wait_handling_stop`]; `Ok(None)` when
    /// nothing is pending.
    ///
    /// # Errors
    /// Returns `Err` if the poll fails.
    pub(crate) fn try_wait_handling_stop(&mut self) -> std::io::Result<Option<WaitOutcome>> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::try_wait_handling_stop(child)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::try_wait_handling_stop(child),
                ChildRepr::RawWindows(child) => child.try_wait_handling_stop(),
            }
        }
    }

    /// Blocking wait that reports both edges of a stop — `Stopped` and
    /// `Continued` — so a holder that tracks "stopped" as a level between
    /// them never has to remember one edge and clear it by convention, the
    /// way [`Self::wait_handling_stop`]'s callers otherwise must.  The
    /// pipeline collector's dedicated external waiter thread is this child's
    /// *sole* waiter, so blocking here costs nothing but that one thread; a
    /// standalone child's own wait must not see `Continued` and keeps polling
    /// the plain variant, since its wait is not the only thing it must do.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    pub(crate) fn wait_tracking_stops(&mut self) -> std::io::Result<WaitOutcome> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::wait_tracking_stops(child)
        }
        #[cfg(windows)]
        {
            // Nothing stops on Windows, so the tracking wait is the plain one.
            match &mut self.0 {
                ChildRepr::Std(child) => windows::wait_handling_stop(child),
                ChildRepr::RawWindows(child) => child.wait_handling_stop(),
            }
        }
    }

    /// Kill this stopped child and reap the terminal status, reporting the
    /// stop that preceded it.  The caller has already decided that this stop
    /// means death; `target` is who the kill addresses.
    ///
    /// # Errors
    /// Returns `Err` if the reaping wait fails.
    #[cfg(unix)]
    pub(crate) fn kill_and_reap_stopped(
        &mut self,
        stopped_by: Signal,
        target: KillTarget,
    ) -> std::io::Result<WaitOutcome> {
        let ChildRepr::Std(child) = &mut self.0;
        unix::kill_and_reap_stopped(child, stopped_by, target)
    }

    /// Blocking reap after a confirmed SIGKILL, which terminates even a stopped
    /// process — so the one raw `Child::wait` inside `ChildHandle` lives here.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    #[allow(clippy::disallowed_methods)]
    pub fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.wait(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }
}

/// Kill a pipeline external stage by pid alone: the reader-gone cascade, and
/// a background group's stop-then-kill fired before the group's own
/// `SIGCONT` could wake it.  By pid rather than through a [`ChildHandle`]
/// because the handle itself lives on the stage's own dedicated waiter
/// thread, the sole owner of its wait.
pub(crate) fn kill_stage_by_pid(pid: u32) {
    #[cfg(unix)]
    unix::kill_stage_by_pid(pid);
    #[cfg(windows)]
    windows::kill_stage_by_pid(pid);
}

/// `SIGCONT` a pipeline external stage by pid alone — the ownerless-stop
/// rule (a detached member deaf to job control revives itself), for the
/// same reason [`kill_stage_by_pid`] is by pid: the `ChildHandle` lives on
/// the stage's own dedicated waiter thread.  `cfg(unix)`: nothing stops on
/// Windows, so there is no stop to revive from there.
#[cfg(unix)]
pub(crate) fn cont_stage_by_pid(pid: u32) {
    unix::cont_stage_by_pid(pid);
}

// ── Escalation ladder ──────────────────────────────────────────────────────

/// Termination signals delivered since the last [`clear`]; the third forces
/// `_exit` in the platform handler.
///
/// Not a delivery mechanism — the handlers deliver through the cancel scopes,
/// which is all [`check`] reads.  A backstop for wedged cooperative delivery
/// must not depend on what it backstops.
pub(crate) static ESCALATION: AtomicU8 = AtomicU8::new(0);

/// Check whether the current evaluation should unwind.
///
/// Fires when the mooring's foreground scope, or any ancestor, is cancelled:
/// a translated signal (SIGINT → foreground `Interrupt`, SIGTERM / SIGHUP →
/// root `Terminate`), a deadline ([`reaper`](crate::process::reaper)), an
/// explicit `cancel <handle>`, or a Ctrl-\ root abort.  The return is `Break`
/// rather than the richer `Control` because a cancellation never carries a
/// tail call, and the error is unspanned: the break path stamps the innermost
/// node it unwinds through.
///
/// # Errors
/// Returns `Err` carrying the strongest cause's message and exit code when the
/// chain is cancelled.
pub fn check(mooring: &crate::types::Mooring) -> Result<(), crate::types::Break> {
    if let Some(cause) = mooring.cancel.cause() {
        return Err(crate::types::Break::Error(crate::types::Error::new(
            cause.message(),
            cause.exit_code(),
        )));
    }
    if let Some(p) = &mooring.park
        && p.gate.is_paused()
    {
        p.gate.wait(mooring.cancel.as_scope()).map_err(|c| {
            crate::types::Break::Error(crate::types::Error::new(c.message(), c.exit_code()))
        })?;
    }
    Ok(())
}

/// Reset the escalation ladder at an acknowledgment boundary — the REPL
/// prompt, a Ctrl-C the line editor absorbed.
///
/// A signal already handled cooperatively thus does not creep the next one
/// toward the force-exit.
pub fn clear() {
    ESCALATION.store(0, Ordering::Relaxed);
}

/// True once a termination signal has landed since the last [`clear`].
///
/// Observability only — nothing gates delivery on it; the tests assert that a
/// cancel path did, or deliberately did not, engage the ladder.
pub fn escalation_pending() -> bool {
    ESCALATION.load(Ordering::Relaxed) >= 1
}

// ── Process-group placement (data) ─────────────────────────────────────────

/// A process-group identifier.
///
/// On Unix a POSIX pgid — the leader's pid, addressable as `kill(-pgid, sig)`
/// to reach every member.  Windows console groups cannot be joined post-spawn,
/// so every external stage leads its own and this carries the *first* stage's
/// pid: the key under which the `windows` module registers the member list and
/// the Job Object that `TerminateJobObject` takes down as one.
///
/// Positive by construction; "no pgid" is `Option<Pgid>`, never a sentinel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pgid(NonZeroI32);

impl Pgid {
    pub fn from_raw(raw: i32) -> Option<Self> {
        NonZeroI32::new(raw)
            .filter(|raw| raw.is_positive())
            .map(Self)
    }

    pub const fn as_raw(self) -> i32 {
        self.0.get()
    }

    #[cfg(unix)]
    pub(crate) const fn from_pid(pid: rustix::process::Pid) -> Self {
        Self(pid.as_raw_nonzero())
    }

    #[cfg(unix)]
    pub const fn as_pid(self) -> rustix::process::Pid {
        // SAFETY: every `Pgid` constructor admits only positive integers.
        unsafe { rustix::process::Pid::from_raw_unchecked(self.as_raw()) }
    }

    /// `SIGKILL` every member — the Job Object's kill on Windows.  Idempotent,
    /// and harmless on a group that has already left.
    pub fn kill(self) {
        #[cfg(unix)]
        self.signal_group(Signal::new(libc::SIGKILL));
        #[cfg(windows)]
        windows::kill_pipeline_group(self);
    }
}

impl std::fmt::Display for Pgid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(unix)]
impl Pgid {
    /// Send `signal` to every process in this group via `kill(-pgid, sig)`.
    ///
    /// Async-signal-safe: one libc call, no allocation, no locking.  Failure is
    /// ignored — the pipeline-abort and Ctrl-Z callers have no recovery, and
    /// `ESRCH` on an already-empty group is the outcome they wanted anyway.
    pub fn signal_group(self, signal: Signal) {
        unsafe {
            libc::kill(-self.as_raw(), signal.number());
        }
    }
}

/// Where a spawned child's process group comes from.
///
/// Foreground job leaders and pipeline first stages take `NewLeader`, later
/// stages `Join`, an interactive background child `Inherit`, a detached worker
/// `NewSession`.  Unix applies the choice in `pre_exec` before `execve`;
/// Windows maps it onto a Job Object at the launch boundary.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Lead a fresh group (`setpgid(0, 0)`), keeping the parent's session and
    /// controlling terminal.
    NewLeader,
    /// Lead a fresh *session* (`setsid`): with no controlling terminal, the
    /// child cannot signal — through the shared tty or `tcgetpgrp` — whatever
    /// owns one.  Its pgid still equals its pid, so `kill(-pgid, …)` still
    /// reaches the subtree.
    NewSession,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

/// Who a signal a `RunningChild` sends addresses.
///
/// Only a child that owns its group outright (`GroupOwner::Standalone`) may
/// have that group signalled; a stage borrowing a pipeline's group signals
/// its own pid alone — the pipeline itself is the only thing that may bring
/// the whole group down. `Pid` also covers a child with no group at all.
#[derive(Clone, Copy, Debug)]
pub(crate) enum KillTarget {
    Group(Pgid),
    Pid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::gate::{StageGate, StagePark, StageStop};
    use std::sync::Arc;

    /// `check` consults the mooring's park after its cause check: a paused
    /// gate blocks the call until `resume`, then returns `Ok`.
    #[test]
    fn check_blocks_while_paused_and_returns_when_resumed() {
        let gate = StageGate::new();
        gate.pause();
        let mut mooring = crate::types::Mooring::adrift();
        mooring.park = Some(StagePark {
            gate: Arc::clone(&gate),
            stop: StageStop::new(),
        });

        let resumer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            gate.resume();
        });

        let start = std::time::Instant::now();
        assert!(check(&mooring).is_ok(), "check must return once resumed");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(40),
            "check must actually have blocked on the pause"
        );
        resumer.join().expect("resumer thread");
    }
}
