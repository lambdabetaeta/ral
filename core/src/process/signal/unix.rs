//! Unix signal handling, process-group placement, and tty ownership — the
//! platform half of `super`.
//!
//! A termination signal unwinds nothing by itself: the handler translates it
//! into a `CancelCause` on the ambient cells that every wait loop already
//! polls, and ticks an escalation ladder whose third delivery forces `_exit`.

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
/// Snapshots the inherited `SIG_IGN` dispositions first, since afterwards ral's
/// own are indistinguishable from the parent's.  Never name SIGWINCH or SIGSEGV
/// here: crossterm's `signal-hook-registry` owns one and fff-search's crash hook
/// may own the other, and a raw `signal(2)` install unhooks it for good.
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
        // `_exit`, not `exit`: atexit hooks run arbitrary code under a handler.
        unsafe { libc::_exit(128 + sig) };
    }
    // Async-signal-safe: atomic read-modify-writes on `static`s.  SIGTERM/SIGHUP
    // land on the durable root, so detached workers hear them too.
    if sig == libc::SIGINT {
        request_foreground_cancel(CancelCause::Interrupt);
    } else {
        request_root_cancel(CancelCause::Terminate);
    }
    // The interactive shell binds SIGINT to `sigint_relay` instead, so SIGINT
    // reaches this fan-out only in batch mode; SIGTERM/SIGHUP always do.
    relay_signal_to_groups(sig);
}

/// The termination handler, for a caller installing it signal by signal.
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

// ── Pipeline relay ─────────────────────────────────────────────────────────
//
// A pipeline with both thread and process stages cannot hand the terminal to
// the external group: its internal stages live in this process and would take
// SIGTTIN.  The terminal stays with the shell and SIGINT is forwarded instead,
// to whichever slots of `RELAY_PGIDS` are claimed.  The handler is installed
// once at startup and never removed, so no signal can arrive mid-install.

const MAX_RELAY: usize = 8;

static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

/// Forward `sig` to every occupied slot's process group; async-signal-safe (a
/// load and a `kill(2)` per slot).
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
    // The watermark reaches only the runs already in flight, so an idle Ctrl-C
    // at the prompt stays the line editor's and detached workers, carrying no
    // birth instant at all, are spared.
    request_foreground_cancel(CancelCause::Interrupt);
    relay_signal_to_groups(libc::SIGINT);
}

/// The SIGINT handler the interactive shell installs in place of `handler`.
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

extern "C" fn sigquit_handler(_: libc::c_int) {
    // Ctrl-\ reaps the session: the root cause reaches the foreground run and
    // every detached worker, and latches on an idle session, being one-way.
    request_root_cancel(CancelCause::RootAbort);
}

/// The SIGQUIT handler behind the interactive "reap everything" gesture.
pub fn quit_handler() -> extern "C" fn(libc::c_int) {
    sigquit_handler
}

/// RAII guard holding a slot in `RELAY_PGIDS` for one mixed pipeline.
pub struct PipelineRelay(usize);

impl PipelineRelay {
    /// Claim a slot for `pgid`, or `None` once `MAX_RELAY` are taken.
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

/// The signals whose startup disposition is snapshotted and later restored in
/// children, listed once so the two consumers cannot drift.  SIGPIPE is
/// deliberately absent: Rust's runtime sets it to `SIG_IGN` at startup, which
/// by the time ral looks would read as the parent's intent.
const MANAGED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGHUP,
];

/// Bitmask of the signals that were `SIG_IGN` when ral started, indexed by
/// signal number — every managed number is under 64, so one `u64` suffices.
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

/// Restore the child's dispositions for the signals ral overrides.  Must run
/// from the post-fork `pre_exec` closure of every external-child spawn.
///
/// `execve(2)` already resets handler pointers; what survives it is `SIG_IGN`,
/// so anything that should stay ignored must be set here explicitly.  That is
/// the POSIX nohup rule — what the parent deliberately ignored has to outlive
/// ral.  Everything else gets `SIG_DFL`, SIGPIPE unconditionally so that a
/// pipeline producer dies when its reader closes (`yes | head`).
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
    /// Apply this policy from inside a post-fork `pre_exec` closure: no
    /// allocation and no stdlib lock (`last_os_error` only reads `errno`).  The
    /// failure must not be swallowed — a child left in the wrong group is
    /// invisible to the SIGINT relay and to the abort path's group-wide SIGTERM.
    /// Reach this through [`spawn_with_pgid`], the single funnel that also
    /// mirrors the call in the parent.
    ///
    /// # Errors
    /// Returns `Err` if `setpgid` / `setsid` fails; `Inherit` makes no syscall.
    pub fn apply(self) -> std::io::Result<()> {
        // `setsid` returns the new sid and `setpgid` 0; both fail with `-1`.
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

#[cfg(test)]
#[allow(
    clippy::cast_possible_wrap,
    reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→i32 reinterpretation never wraps"
)]
fn child_pid(child: &std::process::Child) -> i32 {
    child.id() as i32
}

/// Spawn `cmd` under the one canonical pre-exec discipline.
///
/// Apply `pgid` then [`reset_child_signals`] in the child, and mirror the
/// `setpgid` in the parent so the placement holds whichever side wins the
/// race.  `NewSession` has no mirror — only a process can `setsid` itself —
/// and rests on its `pre_exec`.  The returned leader pgid (`None` only for
/// `Inherit`) is the whole registration: callers keep it for a later wait
/// or for the pipeline group.
///
/// `pre_exec` closures run in registration order, so a caller's own hook
/// (sandbox `RLIMIT`, `2>&1` dup2) runs *before* this one — deliberately, since
/// the signal reset should be the last thing standing before `execve`.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid`, or the `fork` / `exec`
/// itself, fails.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    spawn_with_pgid_after(cmd, pgid, || Ok(()))
}

/// [`spawn_with_pgid`] plus one caller hook, run in the child after the
/// signal reset and before `execve`.
///
/// The hook must therefore keep to async-signal-safe work such as `read`,
/// `close`, or `dup2` on already-open fds.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid` fails, if `after` returns
/// an error, or if the `fork` / `exec` itself fails.
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
    // The mirror's result is ignored: either the child already applied the
    // policy, and a failure here is the benign post-`execve` `EACCES` race, or
    // its `pre_exec` failed and the spawn above already returned that error.
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = Pid::from_child(&child);
            let _ = rustix::process::setpgid(Some(pid), Some(pid));
            Some(Pgid::from_pid(pid))
        }
        PgidPolicy::NewSession => {
            // The new session's pgid equals the child's pid.
            Some(Pgid::from_pid(Pid::from_child(&child)))
        }
        PgidPolicy::Join(group) => {
            let _ = rustix::process::setpgid(Some(Pid::from_child(&child)), Some(group.as_pid()));
            Some(group)
        }
    };
    Ok((child, leader))
}

/// Spawn `cmd` so that the surviving process is this process's
/// *grandchild*, and return its pid.
///
/// The hook of [`spawn_with_pgid_after`] forks again; the intermediate writes
/// the grandchild's pid down a pipe and `_exit`s, and is reaped below — already
/// dead, so that wait cannot block and no zombie exists — leaving the
/// grandchild it orphans to be reparented onto init.
///
/// **The intermediate's leader [`Pgid`] is dropped on the floor, and that
/// discard is the point:** nothing in this process holds a pgid naming the
/// survivor, so no teardown path can reach it with `kill(-pgid, …)`, and its
/// own `setsid` leaves pid == pgid == sid so the dead intermediate's recyclable
/// pid cannot name the group either.
///
/// The survivor keeps no descriptor back to us, the handshake fd being re-armed
/// close-on-exec, so its standard streams are the caller's to point somewhere
/// that outlives this process.  `Ok` still proves the *grandchild* exec'd:
/// `std`'s close-on-exec errno pipe is read until every copy shuts, and the
/// grandchild's shuts only on a successful `execve`.
///
/// # Errors
/// Returns `Err` if either fork, the `setsid`, or the `execve` fails, if
/// the intermediate exits non-zero, or if the pid handshake comes up short.
pub fn spawn_detached(cmd: &mut std::process::Command) -> std::io::Result<u32> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let (mut receipt, handshake) = os_pipe::pipe()?;
    let fd = handshake.as_raw_fd();
    let (mut intermediate, _its_pgid) =
        spawn_with_pgid_after(cmd, PgidPolicy::NewSession, move || {
            // Async-signal-safe throughout: fork, write, _exit, setsid, fcntl.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if pid > 0 {
                let bytes = pid.to_ne_bytes();
                unsafe {
                    let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                    libc::_exit(0);
                }
            }
            if unsafe { libc::setsid() } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // `std` dup2'd the stdio fds before this hook ran and dup2 clears
            // FD_CLOEXEC on its target, so re-arm rather than trust `os_pipe`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })?;
    drop(handshake);
    let born = wait_handling_stop(&mut intermediate, None, false)?;
    if born != crate::process::WaitOutcome::Exited(0) {
        return Err(std::io::Error::other(format!(
            "could not detach: the intermediate process ended as {born:?} instead of exiting 0, so nothing here knows the pid of what it started"
        )));
    }
    let mut pid = [0u8; size_of::<libc::pid_t>()];
    receipt.read_exact(&mut pid).map_err(|err| {
        std::io::Error::other(format!(
            "could not detach: the process was started but its pid never came back ({err}); it may be running, with nothing left to name it"
        ))
    })?;
    u32::try_from(libc::pid_t::from_ne_bytes(pid)).map_err(|_| {
        std::io::Error::other("could not detach: the pid handed back is not a process id")
    })
}

// ── Wait handling for stopped children ─────────────────────────────────────
//
// `Child::wait()` returns only on termination, so a child stopped by SIGTSTP,
// SIGSTOP or SIGTTIN hangs ral with the tty still owned by the stopped pgid;
// `WaitOptions::UNTRACED` makes the wait return on a stop as well.  With
// `park_on_stop` and a tracked pgid the stop surfaces as `Stopped`, which
// `RunningChild::wait` turns into an `Escape::Stopped` for the REPL to park in
// its job table and `fg` to resume.  Otherwise the group is SIGKILLed and
// reaped as `StoppedThenKilled`.

/// Wait for `child` to terminate or stop, classifying a stop per the two modes
/// above.
pub(super) fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
    park_on_stop: bool,
) -> std::io::Result<crate::process::WaitOutcome> {
    let pid = Pid::from_child(child);
    let (_, status) = waitpid_eintr(pid, WaitOptions::UNTRACED)?;
    classify_wait_status(pid, status, pgid, park_on_stop, child)
}

/// Wait for `pid`, retrying after `EINTR` so a signal delivery can never be
/// mistaken for `ECHILD` and flip a live child to "gone".  `NOHANG` is excluded
/// by construction; [`try_waitpid_eintr`] owns the optional result.
fn waitpid_eintr(pid: Pid, options: WaitOptions) -> rustix::io::Result<(Pid, WaitStatus)> {
    wait_blocking_eintr(|| {
        rustix::process::waitpid(Some(pid), options.difference(WaitOptions::NOHANG))
    })
}

/// Poll `pid`, retrying after `EINTR`.
fn try_waitpid_eintr(
    pid: Pid,
    options: WaitOptions,
) -> rustix::io::Result<Option<(Pid, WaitStatus)>> {
    rustix::io::retry_on_intr(|| rustix::process::waitpid(Some(pid), options | WaitOptions::NOHANG))
}

/// Wait for a member of `pgid`, retrying after `EINTR`.  `NOHANG` is excluded
/// by construction; [`try_waitpgid_eintr`] owns the optional result.
///
/// # Errors
/// Returns any terminal wait error, including `ECHILD`.
pub fn waitpgid_eintr(pgid: Pgid, options: WaitOptions) -> rustix::io::Result<(Pid, WaitStatus)> {
    wait_blocking_eintr(|| {
        rustix::process::waitpgid(pgid.as_pid(), options.difference(WaitOptions::NOHANG))
    })
}

/// Poll a member of `pgid`, retrying after `EINTR`; `Ok(None)` means no member
/// has a requested status ready.
///
/// # Errors
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

/// Non-blocking peer of `wait_handling_stop`, returning `Ok(None)` when nothing
/// is pending.  It must keep `UNTRACED`: without it a SIGSTOP'd child reads as
/// "still running" and the pre-wait poll in `RunningChild::wait` spins forever.
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

/// Translate a `waitpid` status into a `WaitOutcome`, shared by the blocking and
/// polling paths.  `WaitStatus` is a total, transparent view of the kernel bits,
/// so termination by a real-time signal classifies with no fallible enum between.
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

/// Park the stopped child when the caller has a job table to resume it from;
/// otherwise SIGKILL its group and reap the terminal status.
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
        // The child raced the kill and exited on its own in the stop→kill
        // window: report that, not a signal the kernel never delivered.
        return Ok(crate::process::WaitOutcome::Exited(code));
    }
    Ok(crate::process::WaitOutcome::NativeCode(status.as_raw()))
}

/// Capture stdin's line-discipline state; `None` when stdin is not a tty.  The
/// restore is the caller's, the right `tcsetattr` flush mode being site-specific.
pub fn termios_snapshot() -> Option<Termios> {
    rustix::termios::tcgetattr(rustix::stdio::stdin()).ok()
}

// ── Foreground ownership ───────────────────────────────────────────────────
//
// A foreground child leaves two pieces of tty state behind.  Miss the process
// group and ral sits in a background pgroup whose next read returns EIO; miss
// the termios and we inherit whatever raw/cooked/ONLCR mode the child last
// wrote — anything calling `cfmakeraw` — for the classic Unix staircase output.

/// RAII guard that snapshots both on acquire and restores both on drop.
///
/// It cannot be constructed without borrowing a [`TerminalLease`]: holding one
/// is the proof that ral owns the controlling terminal's foreground, and only a
/// run whose terminal policy grants the handoff can obtain the borrow
/// (`Shell::terminal_lease`).
///
/// [`TerminalLease`]: crate::process::TerminalLease
pub struct ForegroundGuard {
    saved_pgid: Pid,
    saved_termios: Option<Termios>,
}

impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid and
    /// termios for the restore.  `None` when the pgid handoff itself fails, so
    /// there is then nothing to restore; a failed termios snapshot is not fatal
    /// and leaves only the pgid half to put back on drop.
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

/// Thread-local guard blocking SIGTTOU while ral takes tty state back.
///
/// POSIX counts `tcsetpgrp` / `tcsetattr` from a background process group as
/// terminal output, and at the default disposition the kernel stops the caller
/// mid-restore — exactly ral's position, since a foreground job puts it in the
/// background by construction.  Parent-local: children have SIGTTOU reset by
/// [`reset_child_signals`].
struct SigttouBlock {
    /// `None` when the block never took, leaving nothing to restore.
    old: Option<SigSet>,
}

impl SigttouBlock {
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
    /// Pgid first, since a missed restore leaves ral in a background pgroup
    /// whose next tty read returns EIO.  Termios second, with `Drain` so the
    /// child's last buffered output leaves under the child's own settings:
    /// `Now` would clobber those bytes' line discipline, and `Flush` would
    /// discard input typed during the child's final frame.
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

/// Deliver the SIGINT that raw mode swallowed, but only to a foreground job
/// that is an *external* child.
///
/// A frontend in raw mode clears `ISIG`, so a keypress no longer makes the
/// kernel SIGINT whichever group owns the tty.  When an external child holds it
/// this recreates that delivery, so the child dies and the evaluator's blocking
/// `wait` returns; when the shell itself holds it there is nothing external to
/// signal and the frontend's cooperative [`check`](super::check) unwinds the
/// in-process eval instead.  Signalling *another* group never ticks ral's
/// escalation ladder, so no third-signal force-exit can follow from this.
pub fn interrupt_foreground_child() {
    let Ok(fg) = rustix::termios::tcgetpgrp(rustix::stdio::stdin()) else {
        return;
    };
    if fg != rustix::process::getpgrp() {
        let _ = rustix::process::kill_process_group(fg, rustix::process::Signal::INT);
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

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
    use crate::process::cancel::{DurableRoot, REQUEST_SERIAL, Serial, clear_root_request};
    use std::sync::{Arc, Barrier};

    // `RELAY_PGIDS` is global and tests share a process, so without this lock
    // they would steal each other's slots.
    static RELAY_TEST_LOCK: Serial = Serial::new();

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
        let _lock = RELAY_TEST_LOCK.lock();

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps"
        )]
        let guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|pgid| PipelineRelay::install(pgid).expect("slot should be free"))
            .collect();
        assert_eq!(active_relay_slots(), MAX_RELAY);

        assert!(PipelineRelay::install(99).is_none());

        drop(guards);
        assert_eq!(active_relay_slots(), 0);
    }

    #[test]
    fn released_slot_is_reusable() {
        let _lock = RELAY_TEST_LOCK.lock();

        let g1 = PipelineRelay::install(1).unwrap();
        drop(g1);
        let g2 = PipelineRelay::install(1).unwrap();
        drop(g2);
        assert_eq!(active_relay_slots(), 0);
    }

    // ── Concurrency stress ─────────────────────────────────────────────────

    #[test]
    fn concurrent_install_drop_stress() {
        let _lock = RELAY_TEST_LOCK.lock();

        const ROUNDS: usize = 500;

        for round in 0..ROUNDS {
            let barrier = Arc::new(Barrier::new(MAX_RELAY));
            let handles: Vec<_> = (0..MAX_RELAY)
                .map(|t| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait();
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
        let _lock = RELAY_TEST_LOCK.lock();

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
                        let _ = PipelineRelay::install(pgid);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Signal translation ─────────────────────────────────────────────────

    /// SIGINT interrupts the run in flight and leaves the durable root — and
    /// with it every detached worker — untouched.
    #[test]
    fn handler_translates_sigint_into_foreground_interrupt() {
        let _relay = RELAY_TEST_LOCK.lock();
        let _serial = REQUEST_SERIAL.lock();
        clear();

        let root = DurableRoot::signal_facing();
        let foreground = root.foreground(&root.worker());

        handler(libc::SIGINT);
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Interrupt),
            "SIGINT must interrupt a facing run's foreground scope"
        );
        assert_eq!(
            root.as_scope().cause(),
            None,
            "SIGINT must not reach the durable root"
        );
        assert!(
            escalation_pending(),
            "a delivered signal must tick the escalation ladder"
        );
        clear();
    }

    /// SIGTERM and SIGHUP are shutdown requests, reaching the foreground run
    /// *and* every detached worker under the durable root.  The watermark is
    /// never stamped: the foreground hears the cause through its root ancestry.
    #[test]
    fn handler_translates_sigterm_and_sighup_into_root_terminate() {
        let _relay = RELAY_TEST_LOCK.lock();
        let _serial = REQUEST_SERIAL.lock();
        clear();
        clear_root_request();

        let root = DurableRoot::signal_facing();
        let foreground = root.foreground(&root.worker());

        handler(libc::SIGTERM);
        assert_eq!(
            root.as_scope().cause(),
            Some(CancelCause::Terminate),
            "SIGTERM must terminate a facing session's durable root"
        );
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Terminate),
            "the foreground observes the termination through its root ancestry"
        );

        handler(libc::SIGHUP);
        assert_eq!(
            root.as_scope().cause(),
            Some(CancelCause::Terminate),
            "SIGHUP is the same shutdown request as SIGTERM"
        );
        clear();
        clear_root_request();
    }

    // ── Signal forwarding ──────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // `Command` + `pre_exec` rather than a bare `fork()`, which is a hazard
        // inside a multithreaded test binary.  `sigint_relay` also raises the
        // ambient interrupt, hence `REQUEST_SERIAL` alongside the relay lock.
        let _lock = RELAY_TEST_LOCK.lock();
        let _serial = REQUEST_SERIAL.lock();

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

        // The parent mirrors `setpgid` to close the race.
        let _ = rustix::process::setpgid(Pid::from_raw(child_pid), Pid::from_raw(child_pid));

        let _relay = PipelineRelay::install(child_pid).expect("slot");
        sigint_relay(libc::SIGINT);

        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait");
        assert!(!status.success(), "child should have been killed by SIGINT");
    }

    // ── Detached birth ─────────────────────────────────────────────────────

    /// A `Command` with all three standard streams pointed away from the
    /// harness — the precondition [`spawn_detached`] states.
    fn detachable(program: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    /// The survivor belongs to nobody here: it leads its own group, no wait
    /// from this process can name it, and — where `/proc` can say so — its
    /// parent is no longer us.
    #[test]
    fn detached_survivor_is_orphaned_and_leads_its_own_group() {
        let mut cmd = detachable("sleep");
        cmd.arg("30");
        let pid = spawn_detached(&mut cmd).expect("detached birth");
        let raw = i32::try_from(pid).expect("a live pid fits an i32");

        assert_eq!(
            unsafe { libc::getpgid(raw) },
            raw,
            "the survivor's own setsid must leave pid == pgid == sid"
        );

        let reaped = unsafe { libc::waitpid(raw, std::ptr::null_mut(), libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(reaped, -1, "the survivor must not be waitable from here");
        assert_eq!(
            errno,
            Some(libc::ECHILD),
            "the survivor is a grandchild: no wait here can name it, and none can leak it as a zombie"
        );

        #[cfg(target_os = "linux")]
        {
            // /proc/<pid>/stat is `pid (comm) state ppid …` and `comm` may
            // itself hold spaces and parens, so read fields past the last ')'.
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("survivor stat");
            let tail = &stat[stat.rfind(')').expect("stat has a comm field") + 1..];
            let ppid: i32 = tail
                .split_whitespace()
                .nth(1)
                .expect("stat has a ppid field")
                .parse()
                .expect("ppid is a number");
            assert_ne!(
                ppid,
                i32::try_from(std::process::id()).expect("our own pid fits an i32"),
                "the intermediate's exit must have reparented the survivor away from us"
            );
        }

        unsafe { libc::kill(raw, libc::SIGKILL) };
    }

    /// The intermediate is reaped inside the birth, leaving the caller no child
    /// to wait for and no zombie to accumulate.
    #[test]
    fn detached_birth_leaves_nothing_to_reap() {
        let mut cmd = detachable("sleep");
        cmd.arg("30");
        let pid = spawn_detached(&mut cmd).expect("detached birth");
        let raw = i32::try_from(pid).expect("a live pid fits an i32");

        assert_eq!(
            unsafe { libc::kill(raw, 0) },
            0,
            "the pid handed back must be the surviving grandchild, not the exited intermediate"
        );
        let reaped = unsafe { libc::waitpid(raw, std::ptr::null_mut(), libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(reaped, -1);
        assert_eq!(errno, Some(libc::ECHILD));

        unsafe { libc::kill(raw, libc::SIGKILL) };
    }

    /// Redirected stdio reaches the file.  There is nothing to wait on —
    /// that is the point of the verb — so the assertion polls.
    #[test]
    fn detached_stdout_lands_in_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("out");
        let mut cmd = detachable("/bin/echo");
        cmd.arg("hello")
            .stdout(std::fs::File::create(&out).expect("create the stdout file"));
        spawn_detached(&mut cmd).expect("detached birth");

        for _ in 0..300 {
            if std::fs::read_to_string(&out).is_ok_and(|text| text == "hello\n") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the survivor's stdout never reached {}", out.display());
    }

    /// A program that does not exist comes back `NotFound`, not as a pid —
    /// the proof that `std`'s close-on-exec errno pipe survives the second
    /// fork, so `Ok` really does mean the grandchild exec'd.
    #[test]
    fn detached_missing_program_reports_not_found() {
        let err = spawn_detached(&mut detachable("ral-no-such-program-exists"))
            .expect_err("a program that does not exist cannot be born");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
