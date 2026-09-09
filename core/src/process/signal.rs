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
use super::reaper::{Watch, watch};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::grace_signal;
#[cfg(unix)]
pub use unix::{
    ForegroundGuard, install_handlers, interrupt_foreground_child, interrupt_handler, quit_handler,
    reset_child_signals, spawn_detached, spawn_with_pgid, spawn_with_pgid_after, term_handler,
    termios_snapshot,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ForegroundGuard, ReapStatus, break_pipeline_group, disown_pipeline_group, install_handlers,
    relay_interrupt, reset_child_signals, try_reap_leader,
};
#[cfg(windows)]
pub(crate) use windows::{
    PreparedGroup, apply_group_active_process_limit, close_prepared_group, is_known_group,
    prepare_group, prepared_job, register_prepared_group, release_win_group,
    set_active_process_limit, wait_leader_blocking,
};

// ── Child handle ───────────────────────────────────────────────────────────
//
// Bare `Child::wait` / `try_wait` omit `WUNTRACED`: a SIGSTOP'd child reads as
// "still running" and `wait` blocks forever.  `clippy.toml` disallows the raw
// pair outside `ChildHandle`; the few exceptions carry an `#[allow]`.

/// A spawned child.
///
/// [`Self::into_watch`] is the door to the reaper, which owns the wait on
/// both platforms; [`Self::reap`] is the blocking wait after a confirmed
/// kill, and [`Self::try_reap`] the non-blocking peer `hatch.rs`'s table
/// polls directly.
pub struct ChildHandle(ChildRepr);

enum ChildRepr {
    #[cfg_attr(not(unix), allow(dead_code))]
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(crate::process::launch::RawChild),
}

impl ChildHandle {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn from_std(child: std::process::Child) -> Self {
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
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.kill(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.kill(),
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stdout(),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
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

    /// Non-blocking reap: `Ok(None)` when nothing has exited yet.  Plain
    /// `try_wait`, no `WUNTRACED`, so a stopped child reads as still
    /// running.  Accepted exception to the one SIGCONT rule: `hatch.rs`'s
    /// table is swept on demand rather than watched, so a `kill -STOP` on a
    /// hatched child is never answered and the child stays stopped.
    ///
    /// # Errors
    /// Returns `Err` if the poll fails.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn try_reap(&mut self) -> std::io::Result<Option<WaitOutcome>> {
        let ChildRepr::Std(child) = &mut self.0;
        child
            .try_wait()
            .map(|opt| opt.map(WaitOutcome::from_exit_status))
    }

    /// Blocking reap after a confirmed SIGKILL, which terminates even a stopped
    /// process — so the one raw `Child::wait` inside `ChildHandle` lives here.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.wait(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }

    /// The one door from a spawned child to a [`Watch`]: consumes the
    /// handle, so no later `wait` on this pid is writable behind the
    /// reaper's back.  The stdio ends must already be taken.
    pub(crate) fn into_watch<E: Send + 'static>(
        self,
        tx: std::sync::mpsc::Sender<E>,
        f: impl FnOnce(WaitOutcome) -> E + Send + 'static,
    ) -> Watch {
        let pid = self.id();
        let watch = watch(pid, tx, f);
        // Dropping a `std::process::Child` neither kills nor reaps on Unix,
        // and on Windows only closes std's own handle — the watch opened its
        // own while registering.
        drop(self);
        watch
    }
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
/// root `Terminate`), a deadline ([`deadline`](crate::process::deadline)), an
/// explicit `cancel <handle>`, or a Ctrl-\ root abort.  The return is `Break`
/// rather than the richer `Control` because a cancellation never carries a
/// tail call, and the error is unspanned: the break path stamps the innermost
/// node it unwinds through.
///
/// Mints through `Error::cancelled`, as every poll point does, so a break
/// carrying a `Status::Cancelled` is the cancel's own whichever point raised
/// it.
///
/// # Errors
/// Returns `Err` carrying the strongest cause's message and exit code when the
/// chain is cancelled.
pub fn check(mooring: &crate::types::Mooring) -> Result<(), crate::types::Break> {
    mooring.cancel.cause().map_or(Ok(()), |cause| {
        Err(crate::types::Break::Error(crate::types::Error::cancelled(
            cause,
        )))
    })
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
    #[cfg_attr(not(any(target_os = "linux", windows, test)), allow(dead_code))]
    pub(crate) fn from_raw(raw: i32) -> Option<Self> {
        NonZeroI32::new(raw)
            .filter(|raw| raw.is_positive())
            .map(Self)
    }

    pub(crate) const fn as_raw(self) -> i32 {
        self.0.get()
    }

    #[cfg(unix)]
    pub(crate) const fn from_pid(pid: rustix::process::Pid) -> Self {
        Self(pid.as_raw_nonzero())
    }

    #[cfg(unix)]
    pub(crate) const fn as_pid(self) -> rustix::process::Pid {
        // SAFETY: every `Pgid` constructor admits only positive integers.
        unsafe { rustix::process::Pid::from_raw_unchecked(self.as_raw()) }
    }

    /// `SIGKILL` every member — the Job Object's kill on Windows.  Idempotent,
    /// and harmless on a group that has already left.
    pub(crate) fn kill(self) {
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
    pub(crate) fn signal_group(self, signal: Signal) {
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
