//! Unix signal handling and process-group machinery.
//!
//! The concerns that interlock here:
//!
//!   * **Termination signals** (SIGINT/SIGTERM/SIGHUP) are translated
//!     into a [`CancelCause`](crate::process::CancelCause) on the published
//!     cancel slots — the same delivery every other cancellation uses — and
//!     tick the escalation ladder whose third delivery forces `_exit`.
//!   * **Pipeline relays** keep the controlling tty with the shell while
//!     mixed pipelines run, fanning Ctrl+C out to every external pgid.
//!   * **Process-group placement** is the discipline applied at fork:
//!     every external child gets `setpgid` + `reset_child_signals` via a
//!     single `pre_exec` funnel.
//!   * **Wait handling** for stopped children polls `waitpid` with
//!     [`WaitOptions::UNTRACED`] so a SIGSTOP'd child is classified
//!     (parked or killed-then-reaped) rather than mistaken for still-running.
//!   * **Foreground / tty ownership** hands the controlling terminal to a
//!     foreground child and restores the pgid and line discipline on drop.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, WaitStatus};
use rustix::termios::{OptionalActions, Termios};

use super::{ESCALATION, Pgid, PgidPolicy};
use crate::process::cancel::{CancelCause, request_foreground_cancel, request_root_cancel};

// ── Termination handler ────────────────────────────────────────────────────

/// Install handlers for SIGINT, SIGTERM, SIGHUP.
///
/// Snapshots inherited
/// `SIG_IGN` dispositions *before* installing ral's own handlers so the
/// nohup rule (preserve dispositions the parent deliberately ignored) can
/// be honored in spawned children — see [`reset_child_signals`].
///
/// SIGWINCH (owned in-process by crossterm's `signal-hook-registry` master
/// handler) and SIGSEGV (claimed by fff-search's crash hook, if installed)
/// must never be named here: a raw install would silently and permanently
/// disconnect that registry's dispatch for the signal.
pub fn install_handlers() {
    snapshot_inherited_ignored();
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

extern "C" fn handler(sig: libc::c_int) {
    let prev = ESCALATION.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // Third signal: force exit. Use _exit to avoid atexit deadlocks.
        unsafe { libc::_exit(128 + sig) };
    }
    // Translate the signal into a cause on the published cancel slots —
    // async-signal-safe (a slot load plus a `fetch_max`).  SIGINT is an
    // interrupt of the current foreground turn; SIGTERM/SIGHUP are a
    // shutdown request and land on the durable root, reaching detached
    // workers too.  Every wait loop already polls its scope, so this is
    // what preempts a blocked external child.
    if sig == libc::SIGINT {
        request_foreground_cancel(CancelCause::Interrupt);
    } else {
        request_root_cancel(CancelCause::Terminate);
    }
    // Forward the same signal to any active pipeline groups so external
    // children die too.  The interactive shell rebinds SIGINT to the relay
    // handler instead, so SIGINT reaches this forwarding only in batch
    // mode; SIGTERM/SIGHUP always reach it.
    relay_signal_to_groups(sig);
}

/// Return the handler function pointer for selective signal installation.
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

// ── Pipeline relay ─────────────────────────────────────────────────────────
//
// When a pipeline has both internal (thread) and external (process) stages,
// we cannot hand the terminal to the external process group — internal threads
// live in the shell process and would receive SIGTTIN.  Instead we keep the
// terminal with the shell and forward SIGINT to every active external group.
//
// `RELAY_PGIDS` is a fixed slot array.  Each `PipelineRelay` claims one slot
// with CAS; the handler iterates all slots and sends to any non-zero entry.
// The handler is installed once at startup and never removed, so there is no
// install/uninstall race.  Beyond the relay, the handler also cancels the
// current turn's foreground scope so an in-process foreground computation
// unwinds on Ctrl-C; between turns that cancel is a no-op and the relay slots
// are typically empty, so an idle Ctrl-C at the prompt is left to the line
// editor.

const MAX_RELAY: usize = 8;

static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

/// Forward `sig` to every occupied relay slot's process group.  Shared by
/// the batch/term handler and the interactive relay; async-signal-safe (a
/// slot load and a `kill(2)` per entry).
fn relay_signal_to_groups(sig: libc::c_int) {
    for slot in &RELAY_PGIDS {
        let pgid = slot.load(Ordering::Acquire);
        if pgid != 0 {
            unsafe {
                libc::kill(-pgid, sig);
            }
        }
    }
}

extern "C" fn sigint_relay(_: libc::c_int) {
    // Cancel the current turn's foreground scope so in-process foreground
    // work unwinds at its next poll; a no-op between turns (idle Ctrl-C at
    // the prompt is still handled by the line editor). Detached workers poll
    // their own scopes, not the foreground, so they are spared.
    request_foreground_cancel(CancelCause::Interrupt);
    relay_signal_to_groups(libc::SIGINT);
}

/// Return the relay handler for installation by the interactive shell.
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

extern "C" fn sigquit_handler(_: libc::c_int) {
    // Ctrl-\ in cooked mode: reap the whole session. Cancel the durable
    // root, reaching the foreground turn and every detached worker. A no-op
    // between turns (null slot), so an idle Ctrl-\ does not core-dump.
    request_root_cancel(CancelCause::RootAbort);
}

/// The SIGQUIT handler for the interactive shell to install for the
/// "reap everything" gesture.
pub fn quit_handler() -> extern "C" fn(libc::c_int) {
    sigquit_handler
}

/// RAII guard: holds a slot in `RELAY_PGIDS` for the duration of a mixed
/// pipeline.  Clearing the slot on drop is the only cleanup needed.
pub struct PipelineRelay(usize);

impl PipelineRelay {
    /// Claim an empty slot and record `pgid`.  Returns `None` if all slots
    /// are full (should not happen in practice; 8 concurrent mixed pipelines).
    pub fn install(pgid: i32) -> Option<Self> {
        for (i, slot) in RELAY_PGIDS.iter().enumerate() {
            if slot
                .compare_exchange(0, pgid, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Self(i));
            }
        }
        None
    }
}

impl Drop for PipelineRelay {
    fn drop(&mut self) {
        RELAY_PGIDS[self.0].store(0, Ordering::Release);
    }
}

// ── Inherited dispositions and child-signal reset ──────────────────────────

/// The signals whose startup disposition we snapshot in
/// [`snapshot_inherited_ignored`] and consult in [`reset_child_signals`].
/// Listed once so the two consumers cannot drift.
///
/// SIGPIPE is deliberately absent: Rust's runtime sets SIGPIPE=IGN at startup
/// so panics on broken-pipe writes are graceful, and that disposition would
/// falsely register as "parent intent" by the time ral reads it.  SIGPIPE is
/// reset unconditionally to `SIG_DFL` — see [`reset_child_signals`].
const MANAGED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGHUP,
];

/// Bitmask of signals that were `SIG_IGN` when ral started, indexed by signal
/// number.  Captured by [`install_handlers`] before any of ral's own
/// dispositions are installed.  Read in [`reset_child_signals`] to honor the
/// POSIX nohup rule: a signal the parent deliberately set to `SIG_IGN` must
/// remain `SIG_IGN` in spawned children.
///
/// All managed signal numbers are < 64, so a single u64 suffices.
static INHERITED_IGNORED: AtomicU64 = AtomicU64::new(0);

fn snapshot_inherited_ignored() {
    let mut mask: u64 = 0;
    for &sig in MANAGED_SIGNALS {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &raw mut old) };
        if rc == 0 && old.sa_sigaction == libc::SIG_IGN {
            mask |= 1u64 << sig;
        }
    }
    INHERITED_IGNORED.store(mask, Ordering::Release);
}

fn was_inherited_ignored(sig: libc::c_int) -> bool {
    let mask = INHERITED_IGNORED.load(Ordering::Acquire);
    (mask & (1u64 << sig)) != 0
}

/// Restore the appropriate disposition for the signals that ral overrides
/// or ignores in its own process.  Must run from the post-fork pre-exec
/// closure of every external-child spawn.
///
/// Without an explicit reset, every spawned external would inherit ral's
/// handler pointers.  `execve(2)` resets handler pointers to `SIG_DFL`
/// automatically, so this would mostly work — but `SIG_IGN` survives
/// `execve`.  Anything whose disposition should be `SIG_IGN` must therefore
/// be set *explicitly* to `SIG_IGN` here.
///
/// The nohup rule: a signal that was already `SIG_IGN` when ral started —
/// recorded in `INHERITED_IGNORED` — must be `SIG_IGN` in our children too.
/// The parent deliberately set it; that intent has to survive ral.  For
/// every other managed signal, `SIG_DFL` is the right disposition.
///
/// SIGPIPE is special-cased to `SIG_DFL` unconditionally.  Rust's runtime
/// sets SIGPIPE=IGN at startup; that's not user intent and must not
/// propagate — pipeline producers need SIGPIPE=DFL to die cleanly when
/// their reader closes (`yes | head`).
pub fn reset_child_signals() {
    for &sig in MANAGED_SIGNALS {
        let target = if was_inherited_ignored(sig) {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        unsafe {
            libc::signal(sig, target);
        }
    }
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

// ── Process-group placement ────────────────────────────────────────────────

impl PgidPolicy {
    /// Apply this policy from inside a `pre_exec` closure.  Safe to call
    /// from the post-fork pre-exec context; no allocation, no stdlib mutex
    /// use (`io::Error::last_os_error` only reads `errno`).
    ///
    /// A failed `setpgid` is an error: a child left in the wrong pgid
    /// would be invisible to the SIGINT relay and the abort path's
    /// group-wide SIGTERM.
    ///
    /// Callers should not invoke `setpgid` directly — [`spawn_with_pgid`]
    /// is the single funnel that installs this policy and mirrors it in
    /// the parent.  Searching for `setpgid` should yield this method plus
    /// the parent-side mirror inside [`spawn_with_pgid`].
    ///
    /// # Errors
    /// Returns `Err` if the `setpgid` / `setsid` syscall fails (`errno` via
    /// `last_os_error`); [`Inherit`](PgidPolicy::Inherit) makes no syscall
    /// and never fails.
    pub fn apply(self) -> std::io::Result<()> {
        // `setsid` returns the new sid (the caller's pid) on success;
        // `setpgid` returns 0.  Both signal failure with `-1`.
        let rc = unsafe {
            match self {
                Self::Inherit => 0,
                Self::NewLeader => libc::setpgid(0, 0),
                Self::NewSession => libc::setsid(),
                Self::Join(group) => libc::setpgid(0, group.as_raw()),
            }
        };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// The child's OS pid as an `i32`.
#[cfg(test)]
#[allow(
    clippy::cast_possible_wrap,
    reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→i32 reinterpretation never wraps"
)]
fn child_pid(child: &std::process::Child) -> i32 {
    child.id() as i32
}

/// Spawn `cmd` with a single, canonical pre-exec discipline:
///
///   1. inside the child (post-fork, pre-exec): apply `pgid`, then
///      [`reset_child_signals`] (with the nohup rule);
///   2. inside the parent (post-spawn): mirror the `setpgid` so the child's
///      pgid is established regardless of which side wins the race.  A
///      `NewSession` child has no such mirror — only the child may `setsid`
///      itself — so it relies on its own `pre_exec`.
///
/// Returns the child plus its leader pgid: `Some` for `NewLeader` /
/// `NewSession` / `Join`, `None` for `Inherit`.  Callers that need the leader pgid for
/// later [`wait_handling_stop`] or for the pipeline group simply read it
/// off the return value — there is no separate registration step.
///
/// Ordering: `pre_exec` closures run in registration order, so any caller-
/// installed `pre_exec` (sandbox `RLIMIT`, `2>&1` dup2) runs *before* the
/// closure this function adds.  That order is intentional: fd plumbing
/// and rlimits are independent of pgid placement, and `reset_child_signals`
/// is the last thing we want to happen before `execve` clears the slate.
///
/// # Errors
/// Returns `Err` if the spawn fails — either the child's `setpgid` / `setsid`
/// in the pre-exec closure, or the `fork` / `exec` itself.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    spawn_with_pgid_after(cmd, pgid, || Ok(()))
}

/// Spawn `cmd` with canonical pgid placement and one caller-supplied
/// post-reset hook in the child.
///
/// The hook runs inside the child, after `setpgid` and signal reset, still
/// before `execve`. Callers must therefore restrict it to async-signal-safe
/// work such as `read`, `close`, or `dup2` on already-open fds.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid` fails, if the `after`
/// hook returns an error, or if the `fork` / `exec` itself fails.
pub fn spawn_with_pgid_after<F>(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
    after: F,
) -> std::io::Result<(std::process::Child, Option<Pgid>)>
where
    F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
{
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            pgid.apply()?;
            reset_child_signals();
            after()?;
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // The mirror's `setpgid` result is deliberately ignored: by the time
    // it runs, either the child already applied the policy (a mirror
    // failure is then the benign post-`execve` `EACCES` race) or the
    // child's `pre_exec` failed and the spawn above returned the error.
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = Pid::from_child(&child);
            let _ = rustix::process::setpgid(Some(pid), Some(pid));
            Some(Pgid::from_pid(pid))
        }
        PgidPolicy::NewSession => {
            // `setsid` can run only in the child — a process cannot move
            // another into a new session — so there is no parent-side
            // mirror to close the race: the child's `pre_exec` is the sole
            // authority.  The new session's pgid equals the child's pid.
            Some(Pgid::from_pid(Pid::from_child(&child)))
        }
        PgidPolicy::Join(group) => {
            let _ = rustix::process::setpgid(Some(Pid::from_child(&child)), Some(group.as_pid()));
            Some(group)
        }
    };
    Ok((child, leader))
}

// ── Wait handling for stopped children ─────────────────────────────────────
//
// `Child::wait()` calls `waitpid(pid, &status, 0)` which only returns on
// termination.  A child stopped by SIGTSTP (Ctrl-Z), SIGSTOP, or SIGTTIN
// stays stopped indefinitely — the wait blocks, the controlling tty stays
// owned by the stopped pgid, and ral hangs.
//
// `wait_handling_stop` uses `waitpid` with `WaitOptions::UNTRACED` so the
// wait returns on stop too.  Behaviour on a stop status depends on whether
// the caller opted into job-control parking:
//
//   * `park_on_stop = true` + `pgid = Some(_)`: return `Stopped` without
//     killing.  The caller (eventually `RunningChild::wait`) surfaces this
//     as `Escape::Stopped`; the REPL parks the pgid in the job table
//     and `fg` later resumes it.  Pump threads detach and clean up when
//     the child eventually completes or is killed.
//   * otherwise: legacy kill-then-reap.  SIGKILL the pgid (or fall back to
//     `child.kill()` when no pgid is tracked), drain the wait, return
//     `StoppedThenKilled`.

/// Wait for `child` to terminate or stop.  See module-level comment above
/// for the two stop-handling modes.
pub(super) fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
    park_on_stop: bool,
) -> std::io::Result<crate::process::WaitOutcome> {
    let pid = Pid::from_child(child);
    let (_, status) = waitpid_eintr(pid, WaitOptions::UNTRACED)?;
    classify_wait_status(pid, status, pgid, park_on_stop, child)
}

/// Wait for `pid`, retrying transparently after `EINTR`.
///
/// `EINTR` never reaches the caller, so a signal delivery cannot be mistaken
/// for `ECHILD` and flip a live child to "gone". `NOHANG` is excluded here by
/// construction; [`try_waitpid_eintr`] owns the optional result.
fn waitpid_eintr(pid: Pid, options: WaitOptions) -> rustix::io::Result<(Pid, WaitStatus)> {
    wait_blocking_eintr(|| {
        rustix::process::waitpid(Some(pid), options.difference(WaitOptions::NOHANG))
    })
}

/// Poll `pid`, retrying transparently after `EINTR`.
fn try_waitpid_eintr(
    pid: Pid,
    options: WaitOptions,
) -> rustix::io::Result<Option<(Pid, WaitStatus)>> {
    rustix::io::retry_on_intr(|| rustix::process::waitpid(Some(pid), options | WaitOptions::NOHANG))
}

/// Wait for a member of `pgid`, retrying transparently after `EINTR`.
///
/// `NOHANG` is excluded by construction; [`try_waitpgid_eintr`] owns the
/// optional result.
///
/// # Errors
///
/// Returns any terminal wait error, including `ECHILD`.
pub fn waitpgid_eintr(pgid: Pgid, options: WaitOptions) -> rustix::io::Result<(Pid, WaitStatus)> {
    wait_blocking_eintr(|| {
        rustix::process::waitpgid(pgid.as_pid(), options.difference(WaitOptions::NOHANG))
    })
}

/// Poll a member of `pgid`, retrying transparently after `EINTR`.
///
/// `Ok(None)` means no member has a requested status ready.
///
/// # Errors
///
/// Returns any terminal wait error, including `ECHILD`.
pub fn try_waitpgid_eintr(
    pgid: Pgid,
    options: WaitOptions,
) -> rustix::io::Result<Option<(Pid, WaitStatus)>> {
    rustix::io::retry_on_intr(|| {
        rustix::process::waitpgid(pgid.as_pid(), options | WaitOptions::NOHANG)
    })
}

fn wait_blocking_eintr(
    wait: impl FnMut() -> rustix::io::Result<Option<(Pid, WaitStatus)>>,
) -> rustix::io::Result<(Pid, WaitStatus)> {
    rustix::io::retry_on_intr(wait)
        .map(|status| status.expect("wait without NOHANG returns a status"))
}

/// Non-blocking variant of [`wait_handling_stop`] for cancel-aware
/// polling: returns `Ok(None)` when nothing is pending.  Crucially uses
/// `WaitOptions::UNTRACED` so a SIGSTOP'd child does not look "still
/// running" — the stop notification is consumed and classified just like in the
/// blocking path (parked when `park_on_stop`, otherwise killed and
/// reaped).  Without this, the pre-wait poll in
/// `RunningChild::wait` would spin forever on a self-stopping child.
pub(super) fn try_wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
    park_on_stop: bool,
) -> std::io::Result<Option<crate::process::WaitOutcome>> {
    let pid = Pid::from_child(child);
    let Some((_, status)) = try_waitpid_eintr(pid, WaitOptions::UNTRACED)? else {
        return Ok(None);
    };
    classify_wait_status(pid, status, pgid, park_on_stop, child).map(Some)
}

/// Translate a `waitpid` status into a [`WaitOutcome`].  Stopped
/// statuses fan out to [`handle_stopped`]; exited / signaled / native
/// statuses map straight through. `WaitStatus` is a total, transparent
/// view of the kernel bits, so it also classifies termination by a real-time
/// signal without a fallible enum in the middle. Shared between the blocking
/// `wait_handling_stop` and the non-blocking `try_wait_handling_stop`.
///
/// [`WaitOutcome`]: crate::process::WaitOutcome
fn classify_wait_status(
    pid: Pid,
    status: WaitStatus,
    pgid: Option<Pgid>,
    park_on_stop: bool,
    child: &mut std::process::Child,
) -> std::io::Result<crate::process::WaitOutcome> {
    if let Some(signal) = status.stopping_signal() {
        let stopped_by = crate::process::Signal::new(signal);
        return handle_stopped(stopped_by, pid, pgid, park_on_stop, child);
    }
    if let Some(code) = status.exit_status() {
        return Ok(crate::process::WaitOutcome::Exited(code));
    }
    if let Some(signal) = status.terminating_signal() {
        return Ok(crate::process::WaitOutcome::Signaled(
            crate::process::Signal::new(signal),
        ));
    }
    Ok(crate::process::WaitOutcome::NativeCode(status.as_raw()))
}

/// Handle a stopped child: either park (when `park_on_stop &&
/// pgid.is_some()`) returning [`WaitOutcome::Stopped`], or SIGKILL the
/// pgid (falling back to the direct pid) and reap the terminal status
/// into [`WaitOutcome::StoppedThenKilled`].
///
/// [`WaitOutcome::Stopped`]: crate::process::WaitOutcome::Stopped
/// [`WaitOutcome::StoppedThenKilled`]: crate::process::WaitOutcome::StoppedThenKilled
fn handle_stopped(
    stopped_by: crate::process::Signal,
    pid: Pid,
    pgid: Option<Pgid>,
    park_on_stop: bool,
    child: &mut std::process::Child,
) -> std::io::Result<crate::process::WaitOutcome> {
    if park_on_stop && pgid.is_some() {
        crate::dbg_trace!(
            "fg",
            "pid {pid} stopped (signal {}); parking pgid {:?}",
            stopped_by.display(),
            pgid
        );
        return Ok(crate::process::WaitOutcome::Stopped(stopped_by));
    }
    crate::dbg_trace!(
        "fg",
        "pid {pid} stopped (signal {}); killing pgid {:?}",
        stopped_by.display(),
        pgid,
    );
    match pgid {
        Some(group) => {
            let _ =
                rustix::process::kill_process_group(group.as_pid(), rustix::process::Signal::KILL);
        }
        None => {
            let _ = child.kill();
        }
    }
    let (_, status) = waitpid_eintr(pid, WaitOptions::empty())?;
    if let Some(signal) = status.terminating_signal() {
        return Ok(crate::process::WaitOutcome::StoppedThenKilled {
            stopped_by,
            killed_by: crate::process::Signal::new(signal),
        });
    }
    if let Some(code) = status.exit_status() {
        // The child raced the kill and exited on its own in the
        // stop→kill window — report the real exit, not a kill signal the
        // kernel never delivered.
        return Ok(crate::process::WaitOutcome::Exited(code));
    }
    Ok(crate::process::WaitOutcome::NativeCode(status.as_raw()))
}

/// Capture stdin's line-discipline state via `tcgetattr`.
///
/// `None` when stdin is not a tty (ENOTTY) or the call otherwise fails.
/// The restore is left to the caller, whose `tcsetattr` flush mode
/// (`Drain` to let the child's last frame drain, `Now` for an immediate
/// hand-back) is site-specific.
pub fn termios_snapshot() -> Option<Termios> {
    rustix::termios::tcgetattr(rustix::stdio::stdin()).ok()
}

// ── Foreground ownership ───────────────────────────────────────────────────
//
// Two pieces of tty state must be returned to the shell when a foreground
// child exits: the foreground process group (`tcsetpgrp`) and the line
// discipline's termios (`tcsetattr`).  Missing the first puts ral into a
// background pgroup whose next read returns EIO; missing the second
// inherits whatever cooked/raw/ONLCR settings the child last wrote — vim,
// less, fzf, `stty -onlcr`, anything calling `cfmakeraw` — yielding the
// classic Unix staircase output or stuck raw mode.  `ForegroundGuard`
// snapshots both on acquire and restores both on Drop.

/// RAII guard for terminal foreground ownership and line-discipline state.
///
/// `try_acquire` performs `tcsetpgrp(stdin, target)`, snapshots the
/// current termios, and remembers the previous foreground pgid; `drop`
/// restores both.  It cannot be invoked without a [`TerminalLease`] borrow:
/// holding `&TerminalLease` is the proof that ral owns the controlling
/// terminal's foreground, replacing the old internal `startup_foreground`
/// re-check.  Returns `None` only when the pgid handoff itself fails (a
/// non-positive target, or `tcsetpgrp` errors) — in which case there is
/// nothing to restore.  Termios snapshot may itself fail (ENOTTY on an
/// exotic fd); that is non-fatal and only the pgid half is restored on Drop.
///
/// [`TerminalLease`]: crate::process::TerminalLease
pub struct ForegroundGuard {
    saved_pgid: Pid,
    saved_termios: Option<Termios>,
}

impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid and
    /// termios for the eventual restore.  Returns `None` when no handoff is
    /// appropriate.
    pub fn try_acquire(target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
        if target <= 0 {
            return None;
        }
        let target = Pid::from_raw(target)?;
        let saved = rustix::process::getpgrp();
        #[cfg_attr(
            not(debug_assertions),
            allow(
                unused_variables,
                reason = "`dbg_trace!` discards its arguments in release builds"
            )
        )]
        if let Err(err) = rustix::termios::tcsetpgrp(rustix::stdio::stdin(), target) {
            crate::dbg_trace!(
                "fg",
                "acquire: tcsetpgrp({}) failed: {err}",
                target.as_raw_nonzero()
            );
            return None;
        }
        let saved_termios = termios_snapshot();
        if saved_termios.is_none() {
            crate::dbg_trace!("fg", "acquire: tcgetattr failed");
        }
        Some(Self {
            saved_pgid: saved,
            saved_termios,
        })
    }
}

/// Thread-local guard that blocks SIGTTOU while ral restores tty ownership.
///
/// POSIX treats `tcsetpgrp`/`tcsetattr` by a background process group as
/// terminal output; with SIGTTOU at its default disposition the kernel stops
/// the caller instead of completing the restore.  Foreground jobs necessarily
/// put ral in the background while they run, so the release path blocks
/// SIGTTOU for the tiny restore window.  Children still get SIGTTOU reset by
/// [`reset_child_signals`]; this guard is parent-local.
struct SigttouBlock {
    /// The pre-block mask; `None` when the block could not be installed,
    /// in which case there is nothing to restore.
    old: Option<SigSet>,
}

impl SigttouBlock {
    /// Block SIGTTOU for the current thread, remembering the previous mask.
    fn new() -> Self {
        let mut block = SigSet::empty();
        block.add(nix::sys::signal::Signal::SIGTTOU);
        Self {
            old: block.thread_swap_mask(SigmaskHow::SIG_BLOCK).ok(),
        }
    }
}

impl Drop for SigttouBlock {
    fn drop(&mut self) {
        if let Some(old) = &self.old {
            let _ = old.thread_set_mask();
        }
    }
}

impl Drop for ForegroundGuard {
    /// Restore the foreground pgid and termios recorded at acquisition.
    ///
    /// Pgid first: a missed restore puts ral into a background pgroup whose
    /// next tty read returns EIO.  The restore blocks SIGTTOU because the
    /// foreground child made ral a background process group by construction;
    /// without that mask a batch launcher (`claude.ral`) is itself stopped
    /// while giving the terminal back.  Termios second, with `Drain` so
    /// the child's last buffered output drains under the child's settings
    /// before ours reapply (`Now` would clobber those bytes' line
    /// discipline; `Flush` would discard input typed during the child's
    /// final frame).  Both syscalls retry on EINTR; persistent failure is
    /// logged via `dbg_trace` so silent-loss is at least observable.
    fn drop(&mut self) {
        let _sigttou = SigttouBlock::new();
        for _ in 0..3 {
            match rustix::termios::tcsetpgrp(rustix::stdio::stdin(), self.saved_pgid) {
                Ok(()) => break,
                Err(err) => {
                    if err != Errno::INTR {
                        crate::dbg_trace!(
                            "fg",
                            "release: tcsetpgrp({}) failed: {err}",
                            self.saved_pgid.as_raw_nonzero()
                        );
                        break;
                    }
                }
            }
        }
        let cur = rustix::termios::tcgetpgrp(rustix::stdio::stdin())
            .map_or(-1, |fg| fg.as_raw_nonzero().get());
        if cur != self.saved_pgid.as_raw_nonzero().get() {
            crate::dbg_trace!(
                "fg",
                "release: tty fg is {cur}, want {} (next tty read may EIO)",
                self.saved_pgid.as_raw_nonzero()
            );
        }
        if let Some(t) = self.saved_termios.as_ref() {
            for _ in 0..3 {
                match rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Drain, t)
                {
                    Ok(()) => return,
                    Err(err) => {
                        if err != Errno::INTR {
                            crate::dbg_trace!("fg", "release: tcsetattr failed: {err}");
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Deliver the SIGINT that raw mode swallowed — but only to a foreground
/// job that is an *external* child, not the shell itself.
///
/// A frontend that puts the terminal in raw mode disables `ISIG`, so a
/// keypress no longer makes the kernel SIGINT whichever process group
/// owns the controlling tty.  When an external child holds the terminal
/// (its pgid differs from ours) this recreates that delivery, so the
/// child dies and the evaluator's blocking `wait` returns.  When the
/// shell itself owns the terminal there is no external process to
/// signal — the frontend's cooperative [`interrupt`](super::interrupt)
/// unwinds the in-process eval instead.
///
/// Signalling *another* group never touches ral's own escalating
/// counter, so this carries no risk of the third-signal force-exit.
pub fn interrupt_foreground_child() {
    let Ok(fg) = rustix::termios::tcgetpgrp(rustix::stdio::stdin()) else {
        return;
    };
    if fg != rustix::process::getpgrp() {
        let _ = rustix::process::kill_process_group(fg, rustix::process::Signal::INT);
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Count of currently occupied relay slots (for testing).
#[cfg(test)]
fn active_relay_slots() -> usize {
    RELAY_PGIDS
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != 0)
        .count()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::super::{clear, escalation_pending};
    use super::*;
    use crate::process::cancel::{
        CancelScope, SLOT_SERIAL, publish_durable_root, publish_foreground,
    };
    use std::sync::{Arc, Barrier, Mutex};

    // All relay tests share a process-wide lock because `RELAY_PGIDS` is a
    // global.  Tests run concurrently in the same process by default;
    // without this they would steal each other's slots.
    static RELAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sigttou_is_blocked() -> bool {
        SigSet::thread_get_mask()
            .expect("pthread_sigmask query failed")
            .contains(nix::sys::signal::Signal::SIGTTOU)
    }

    #[test]
    fn sigttou_block_restores_signal_mask() {
        let was_blocked = sigttou_is_blocked();
        {
            let _block = SigttouBlock::new();
            assert!(sigttou_is_blocked());
        }
        assert_eq!(sigttou_is_blocked(), was_blocked);
    }

    // ── Slot allocation ────────────────────────────────────────────────────

    #[test]
    fn slots_fill_and_drain() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        // Claim all 8 slots with distinct pgids.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps"
        )]
        let guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|pgid| PipelineRelay::install(pgid).expect("slot should be free"))
            .collect();
        assert_eq!(active_relay_slots(), MAX_RELAY);

        // A 9th install must fail.
        assert!(PipelineRelay::install(99).is_none());

        // Drop all; every slot must be released.
        drop(guards);
        assert_eq!(active_relay_slots(), 0);
    }

    #[test]
    fn released_slot_is_reusable() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        let g1 = PipelineRelay::install(1).unwrap();
        drop(g1);
        // The same pgid should be installable again immediately.
        let g2 = PipelineRelay::install(1).unwrap();
        drop(g2);
        assert_eq!(active_relay_slots(), 0);
    }

    // ── Concurrency stress ─────────────────────────────────────────────────

    #[test]
    fn concurrent_install_drop_stress() {
        // 8 threads race to claim all slots simultaneously, hold briefly,
        // release.  Repeat 500 times.  No slot should ever be double-claimed
        // or leaked.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        const ROUNDS: usize = 500;

        for round in 0..ROUNDS {
            let barrier = Arc::new(Barrier::new(MAX_RELAY));
            let handles: Vec<_> = (0..MAX_RELAY)
                .map(|t| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait(); // all threads start together
                        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps")]
                        let pgid = (round * MAX_RELAY + t + 1) as i32;
                        let g = PipelineRelay::install(pgid)
                            .expect("slot unavailable — possible double-claim");
                        std::thread::yield_now();
                        drop(g);
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(active_relay_slots(), 0, "slot leak after round {round}");
        }
    }

    #[test]
    fn overflow_returns_none_not_panic() {
        // Fill all slots, then hammer install from many threads
        // simultaneously.  Every extra install must return None, never panic
        // or corrupt state.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps"
        )]
        let _guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|p| PipelineRelay::install(p).unwrap())
            .collect();

        let handles: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| {
                    for pgid in 100..200i32 {
                        let _ = PipelineRelay::install(pgid); // must not panic
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Signal translation ─────────────────────────────────────────────────

    /// The batch/term handler translates SIGINT into a foreground
    /// [`Interrupt`](crate::process::CancelCause::Interrupt): the current
    /// turn unwinds (and its blocked external wakes at the next scope
    /// poll), while the durable root — and with it every detached
    /// worker — is left untouched.  The delivery also ticks the
    /// escalation ladder, which is what backs the third-signal `_exit`.
    #[test]
    fn handler_translates_sigint_into_foreground_interrupt() {
        let _relay = RELAY_TEST_LOCK.lock().unwrap();
        let _slots = SLOT_SERIAL.lock().unwrap();
        clear();

        let foreground = CancelScope::root();
        let root = CancelScope::root();
        let _fg = publish_foreground(&foreground);
        let _rt = publish_durable_root(&root);

        handler(libc::SIGINT);
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Interrupt),
            "SIGINT must interrupt the published foreground scope"
        );
        assert_eq!(root.cause(), None, "SIGINT must not reach the durable root");
        assert!(
            escalation_pending(),
            "a delivered signal must tick the escalation ladder"
        );
        clear();
    }

    /// SIGTERM and SIGHUP are shutdown requests: the handler translates
    /// them into a root [`Terminate`](crate::process::CancelCause::Terminate),
    /// reaching the foreground turn *and* every detached worker parented
    /// under the durable root — the semantics a `timeout(1)`- or
    /// systemd-style SIGTERM expects.  The foreground slot itself is not
    /// written; the foreground observes the cause through its root
    /// ancestry.
    #[test]
    fn handler_translates_sigterm_and_sighup_into_root_terminate() {
        let _relay = RELAY_TEST_LOCK.lock().unwrap();
        let _slots = SLOT_SERIAL.lock().unwrap();
        clear();

        let root = CancelScope::root();
        let foreground = root.child();
        let _fg = publish_foreground(&foreground);
        let _rt = publish_durable_root(&root);

        handler(libc::SIGTERM);
        assert_eq!(
            root.cause(),
            Some(CancelCause::Terminate),
            "SIGTERM must terminate the published durable root"
        );
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Terminate),
            "the foreground observes the termination through its root ancestry"
        );

        handler(libc::SIGHUP);
        assert_eq!(
            root.cause(),
            Some(CancelCause::Terminate),
            "SIGHUP is the same shutdown request as SIGTERM"
        );
        clear();
    }

    // ── Signal forwarding ──────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // Spawn `sleep 1000` as a child in its own process group (via
        // pre_exec).  Claim a relay slot for the child's pgid and call
        // sigint_relay directly — equivalent to the shell receiving SIGINT
        // while the pipeline runs.  Verify the child was killed by the
        // signal.
        //
        // We use Command + pre_exec rather than fork() to avoid the hazards
        // of forking inside a multithreaded test binary.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("1000");
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        let child_pid = child_pid(&child);

        // Parent mirrors setpgid to close the race.
        let _ = rustix::process::setpgid(Pid::from_raw(child_pid), Pid::from_raw(child_pid));

        // Claim relay slot and fire the handler directly.
        let _relay = PipelineRelay::install(child_pid).expect("slot");
        sigint_relay(libc::SIGINT);

        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait");
        // sleep was killed by SIGINT; exit code should be non-zero.
        assert!(!status.success(), "child should have been killed by SIGINT");
    }
}
