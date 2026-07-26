//! Signal handling and process-group placement — facade.
//!
//! Two concerns sit behind the same module name:
//!
//!   * **Signal translation.**  SIGINT / SIGTERM / SIGHUP are translated
//!     by the platform handlers into a [`CancelCause`](crate::process::CancelCause)
//!     on the published cancel slots — SIGINT interrupts the foreground run,
//!     SIGTERM / SIGHUP terminate the durable root.  A separate escalation
//!     ladder counts deliveries and forces `_exit` on the third — the last
//!     resort when cooperative delivery is wedged, never the delivery
//!     mechanism itself.  The cancel primitives themselves live in
//!     [`cancel`](crate::process::cancel).
//!   * **Process-group placement.**  [`PgidPolicy`] is the type-level
//!     spelling of "stay in the parent's group / become a leader of a
//!     fresh group / join an existing group as a non-leader" applied via
//!     `pre_exec` before `execve` at spawn time.
//!
//! Platform-specific machinery lives in `unix` and `windows` — each a module
//! that exists only on its own platform, so neither can be linked from a page
//! the other one builds; this
//! file re-exports the items each platform provides and keeps the
//! portable data types and the unwind-poll path here.

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

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ForegroundGuard, PipelineRelay, ReapStatus, apply_group_active_process_limit,
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
// Bare `std::process::Child::wait` / `try_wait` call `waitpid` without
// stop notifications: a SIGSTOP'd child is invisible to `try_wait` (it
// reports "still running"), and `wait` blocks indefinitely. Every external
// child ral tracks therefore goes through `ChildHandle`, whose wait methods
// request stopped statuses on Unix and classify the result. Direct
// `Child::wait` / `try_wait` calls outside this file are blocked by
// `clippy::disallowed_methods` (see `clippy.toml`); the few legitimate
// exceptions are tagged with `#[allow]` at the call site.

/// Wrapper around a spawned child that hides the raw `wait` / `try_wait`
///
/// and only exposes [`Self::wait_handling_stop`] /
/// [`Self::try_wait_handling_stop`] — the stop-aware peers that can't
/// misclassify a stopped child as "still running".
pub struct ChildHandle(ChildRepr);

enum ChildRepr {
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(crate::process::launch::RawChild),
}

impl ChildHandle {
    /// Wrap a freshly-spawned child.  After this point the std handle
    /// is no longer reachable by name; the only paths to wait are the
    /// `*_handling_stop` methods.
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

    /// Kill the child (SIGKILL on Unix, `TerminateProcess` on Windows).
    ///
    /// # Errors
    /// Returns `Err` if the underlying platform kill fails.
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

    /// Blocking wait that observes stopped statuses on Unix. See the platform
    /// `wait_handling_stop` for the park / kill-and-reap dispatch on stop.
    ///
    /// # Errors
    /// Returns `Err` if the underlying `waitpid` (Windows: the wait, or the
    /// kill-and-reap on a stop) fails.
    pub fn wait_handling_stop(
        &mut self,
        pgid: Option<Pgid>,
        park_on_stop: bool,
    ) -> std::io::Result<WaitOutcome> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::wait_handling_stop(child, pgid, park_on_stop)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::wait_handling_stop(child, pgid, park_on_stop),
                ChildRepr::RawWindows(child) => child.wait_handling_stop(),
            }
        }
    }

    /// Non-blocking peek matching the contract of
    /// [`Self::wait_handling_stop`]: returns `Ok(None)` when nothing is
    /// pending, otherwise classifies and (on no-park stop) kills and
    /// reaps inline.
    ///
    /// # Errors
    /// Returns `Err` if the underlying `waitpid` (Windows: the poll, or the
    /// kill-and-reap on a stop) fails.
    pub fn try_wait_handling_stop(
        &mut self,
        pgid: Option<Pgid>,
        park_on_stop: bool,
    ) -> std::io::Result<Option<WaitOutcome>> {
        #[cfg(unix)]
        {
            let ChildRepr::Std(child) = &mut self.0;
            unix::try_wait_handling_stop(child, pgid, park_on_stop)
        }
        #[cfg(windows)]
        {
            match &mut self.0 {
                ChildRepr::Std(child) => windows::try_wait_handling_stop(child, pgid, park_on_stop),
                ChildRepr::RawWindows(child) => child.try_wait_handling_stop(),
            }
        }
    }

    /// Blocking reap used only after a confirmed SIGKILL on the abort
    /// path: SIGKILL terminates even stopped processes, so the full
    /// stop-aware ceremony of `wait_handling_stop` is pure overhead
    /// here.  This is the one place inside `ChildHandle` that calls
    /// the raw `Child::wait`.
    ///
    /// # Errors
    /// Returns `Err` if the underlying `wait` fails.
    #[allow(clippy::disallowed_methods)]
    pub fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.wait(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }
}

// ── Escalation ladder ──────────────────────────────────────────────────────

/// Count of termination signals delivered since the last [`clear`]
/// boundary.  The third delivery forces `_exit` in the platform handler.
///
/// This is *not* a delivery mechanism: the handlers deliver by
/// translating each signal into a [`CancelCause`](crate::process::CancelCause)
/// on the published cancel slots, and [`check`] reads only the scope tree.
/// The ladder is
/// the last resort for a process whose cooperative delivery is wedged —
/// it must stay independent of the thing it backstops.
pub(crate) static ESCALATION: AtomicU8 = AtomicU8::new(0);

/// Check whether the current evaluation should unwind.
///
/// Fires when the shell's [`CancelScope`](crate::process::CancelScope) (or any
/// of its ancestors) has been cancelled: a signal translated by the platform
/// handler (SIGINT → foreground `Interrupt`, SIGTERM / SIGHUP → root
/// `Terminate`), a run deadline ([`reaper`](crate::process::reaper)), an
/// explicit `cancel <handle>`, or a Ctrl-\ root abort.  The error carries the
/// strongest cause's message and exit code.
///
/// Cancellations are interrupts: they surface as either a recoverable
/// error (`Break::Error`) or a non-catchable escape (`Break::Escape`).
/// They never carry a tail call, hence the `Break` return type rather
/// than the richer `Control`.
///
/// # Errors
/// Returns `Err` if the shell's cancel scope (or any ancestor) is
/// cancelled, carrying the strongest cause's message and exit code; `Ok(())`
/// while the chain is uncancelled.
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::Break> {
    if let Some(cause) = shell.run.cancel.cause() {
        return Err(crate::types::Break::Error(
            crate::types::Error::new(cause.message(), cause.exit_code())
                .at_loc(shell.run.loc.source_loc(0)),
        ));
    }
    Ok(())
}

/// Reset the escalation ladder at an acknowledgment boundary — a fresh
/// prompt, a run compile, a session reboot —
///
/// so signals already handled
/// cooperatively don't creep toward the third-delivery force-exit.
pub fn clear() {
    ESCALATION.store(0, Ordering::Relaxed);
}

/// True once a termination signal has been delivered since the last
/// [`clear`] — the escalation ladder has at least one tick.
///
/// Observability
/// only: nothing gates delivery on this (hosts and tests use it to prove a
/// cancel path did, or deliberately did not, engage the ladder).
pub fn escalation_pending() -> bool {
    ESCALATION.load(Ordering::Relaxed) >= 1
}

// ── Fallback platform stubs ────────────────────────────────────────────────
//
// `cfg(not(any(unix, windows)))` (wasm, etc.) — keep the compile path open
// without trying to emulate process-group semantics.

#[cfg(not(any(unix, windows)))]
pub fn install_handlers() {}

#[cfg(not(any(unix, windows)))]
pub fn reset_child_signals() {}

#[cfg(not(any(unix, windows)))]
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    _pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    let child = cmd.spawn()?;
    Ok((child, None))
}

#[cfg(not(any(unix, windows)))]
pub fn spawn_with_pgid_after<F>(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
    _after: F,
) -> std::io::Result<(std::process::Child, Option<Pgid>)>
where
    F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
{
    spawn_with_pgid(cmd, pgid)
}

#[cfg(not(any(unix, windows)))]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
    _park_on_stop: bool,
) -> std::io::Result<WaitOutcome> {
    child.wait().map(WaitOutcome::from_exit_status)
}

#[cfg(not(any(unix, windows)))]
pub struct PipelineRelay;

#[cfg(not(any(unix, windows)))]
impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        None
    }
}

#[cfg(not(any(unix, windows)))]
pub struct ForegroundGuard;

#[cfg(not(any(unix, windows)))]
impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
        None
    }
}

// ── Process-group placement (data) ─────────────────────────────────────────

/// Newtype wrapping a process-group identifier.
///
/// On Unix: a POSIX pgid (== leader's pid).  Addressable via
/// `kill(-pgid, sig)` to fan a signal out across every member.
///
/// On Windows: the leader's pid.  Windows console process groups can't
/// be joined post-spawn — every external pipeline stage is its own
/// group leader (spawned with `CREATE_NEW_PROCESS_GROUP`).  The `Pgid`
/// here is the *first* stage's pid; the `PipelineGroup` keeps the full
/// member list and a Job Object that ties them together for abort-time
/// `TerminateJobObject`.
///
/// Constructed only from a positive pid; no integer sentinel encodes "no
/// pgid" — the `Option<Pgid>` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pgid(NonZeroI32);

impl Pgid {
    /// Construct a process-group id from its positive kernel spelling.
    pub fn from_raw(raw: i32) -> Option<Self> {
        NonZeroI32::new(raw)
            .filter(|raw| raw.is_positive())
            .map(Self)
    }

    /// Return the positive kernel spelling of this process-group id.
    pub const fn as_raw(self) -> i32 {
        self.0.get()
    }

    #[cfg(unix)]
    pub(crate) const fn from_pid(pid: rustix::process::Pid) -> Self {
        Self(pid.as_raw_nonzero())
    }

    /// View this group identifier as the positive pid rustix uses for pgids.
    #[cfg(unix)]
    pub const fn as_pid(self) -> rustix::process::Pid {
        // SAFETY: `Pgid::from_raw` admits only positive integers.
        unsafe { rustix::process::Pid::from_raw_unchecked(self.as_raw()) }
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
    /// Async-signal-safe: no allocation, no locking, just one libc call.
    /// Failures (e.g. `ESRCH` on a group whose last member has already
    /// exited) are silently ignored — pipeline abort and Ctrl-Z paths
    /// don't have a meaningful recovery for those, and the kernel has
    /// already done the right thing if the group is gone.
    pub fn signal_group(self, signal: Signal) {
        unsafe {
            libc::kill(-self.as_raw(), signal.number());
        }
    }
}

/// Process-group placement decision applied via `pre_exec` before `execve`.
///
/// Foreground job leaders and pipeline first stages use `NewLeader`;
/// subsequent pipeline stages use `Join(leader_pgid)`; an interactive
/// background child that must share the terminal's signal reach uses
/// `Inherit`; a *detached* background worker (a `spawn`/non-interactive
/// top-level child that never takes the foreground) uses `NewSession`.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Become the leader of a fresh process group (`setpgid(0, 0)`),
    /// keeping the parent's session and controlling terminal.
    NewLeader,
    /// Become the leader of a fresh *session* (`setsid`): a new process
    /// group with **no controlling terminal**.  A detached background
    /// worker has no business holding the terminal, and severing the
    /// session is what stops it from signalling — via the shared tty or
    /// `tcgetpgrp` — whatever owns the controlling terminal (the REPL, or
    /// a supervising frontend).  The new session's pgid equals the child's
    /// pid, so the watchdog's `kill(-pgid, …)` still reaps the subtree.
    NewSession,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}
