//! Unix signal handling, process-group placement, and tty ownership — the
//! platform half of `super`.
//!
//! A termination signal unwinds nothing by itself: the handler translates it
//! into an ambient cause its host forwards to the engine as `Control`, and
//! ticks an escalation ladder whose third delivery forces `_exit`.

use std::sync::atomic::{AtomicU64, Ordering};

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::io::Errno;
use rustix::process::Pid;
use rustix::termios::{OptionalActions, Termios};

use super::{ESCALATION, Pgid, PgidPolicy};
use crate::process::Signal;
use crate::process::cancel::{CancelCause, request_interrupt, request_root_cancel};

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
    crate::process::reaper::ensure_installed();
}

extern "C" fn handler(sig: libc::c_int) {
    let prev = ESCALATION.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // `_exit`, not `exit`: atexit hooks run arbitrary code under a handler.
        unsafe { libc::_exit(forced_exit_code(sig)) };
    }
    // Async-signal-safe: atomic read-modify-writes on `static`s.  SIGTERM/SIGHUP
    // land on the durable root, so detached workers hear them too.
    if sig == libc::SIGINT {
        request_interrupt();
    } else {
        request_root_cancel(CancelCause::Terminate);
    }
}

/// The status a forced exit on `sig` reports: its gesture's cause's, or, for a
/// signal no gesture names, the bare `128 + sig`.  Pure, so async-signal-safe.
fn forced_exit_code(sig: libc::c_int) -> i32 {
    gesture(Signal::new(sig)).map_or(128 + sig, |cause| {
        crate::types::Status::Cancelled(cause).code()
    })
}

/// The termination handler, for a caller installing it signal by signal.
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

extern "C" fn sigint_handler(_: libc::c_int) {
    // Forwarded as `Control::Interrupt`, which strikes only the dispatch in
    // flight: an idle Ctrl-C stays the line editor's, and detached workers are
    // spared.
    request_interrupt();
}

/// The SIGINT handler the interactive shell installs in place of `handler`:
/// non-escalating, and no delivery of its own — a run in flight hears the
/// interrupt through its host's `Control`.
pub fn interrupt_handler() -> extern "C" fn(libc::c_int) {
    sigint_handler
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
    /// invisible to every teardown that addresses the group as a whole.
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
    let child = crate::process::spawn(cmd)?;
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

    let (mut receipt, handshake) = crate::process::cloexec_pipe()?;
    let fd = handshake.as_raw_fd();
    let (intermediate, _its_pgid) =
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
    let pid = intermediate.id();
    let (tx, rx) = std::sync::mpsc::channel();
    let watch = crate::process::reaper::watch(pid, tx, std::convert::identity);
    // Dropping a `std::process::Child` neither kills nor reaps: the watch
    // above is what now owns its wait.
    drop(intermediate);
    let born = rx.recv().map_err(|_| {
        std::io::Error::other("could not detach: the reaper never reported the intermediate's exit")
    })?;
    watch.reap()?;
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

/// The catchable signal a teardown opens with before its SIGKILL; `None` skips
/// straight to the kill — a reader-gone cut must not hand the verdict back to
/// the producer's own disposition, and a root abort has no grace to offer.
pub(crate) fn grace_signal(cause: CancelCause) -> Option<Signal> {
    match cause {
        CancelCause::Interrupt => Some(Signal::new(libc::SIGINT)),
        CancelCause::Explicit | CancelCause::Deadline | CancelCause::Terminate => {
            Some(Signal::new(libc::SIGTERM))
        }
        CancelCause::ReaderGone | CancelCause::RootAbort => None,
    }
}

/// The cause each gesture signal stands for, as the shell's own handler reads it.
const GESTURES: [(i32, CancelCause); 4] = [
    (libc::SIGINT, CancelCause::Interrupt),
    (libc::SIGQUIT, CancelCause::RootAbort),
    (libc::SIGTERM, CancelCause::Terminate),
    (libc::SIGHUP, CancelCause::Terminate),
];

/// The cause a gesture signal stands for, whoever delivered it.  Pure, so
/// async-signal-safe.
pub(crate) fn gesture(signal: Signal) -> Option<CancelCause> {
    GESTURES
        .into_iter()
        .find_map(|(number, cause)| (number == signal.number()).then_some(cause))
}

/// The signals ral's own teardown for `cause` sends: its grace signal, then
/// the SIGKILL that ends every teardown.
pub(crate) fn teardown_signals(cause: CancelCause) -> impl Iterator<Item = Signal> {
    grace_signal(cause)
        .into_iter()
        .chain([Signal::new(libc::SIGKILL)])
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
    pub(crate) fn try_acquire(target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::super::{clear, escalation_pending};
    use super::*;
    use crate::process::cancel::{REQUEST_SERIAL, clear_root_request};

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

    // ── Teardown ladder ────────────────────────────────────────────────────

    /// The catchable opening of every teardown, and the two causes that have
    /// none to offer.
    #[test]
    fn grace_signal_opens_on_the_cause() {
        for (cause, expected) in [
            (CancelCause::Interrupt, Some(libc::SIGINT)),
            (CancelCause::Explicit, Some(libc::SIGTERM)),
            (CancelCause::Deadline, Some(libc::SIGTERM)),
            (CancelCause::Terminate, Some(libc::SIGTERM)),
            (CancelCause::ReaderGone, None),
            (CancelCause::RootAbort, None),
        ] {
            assert_eq!(
                grace_signal(cause).map(Signal::number),
                expected,
                "{cause:?} must open its teardown with {expected:?}"
            );
        }
    }

    // ── Signal translation ─────────────────────────────────────────────────

    /// A listener on the ambient causes, and how long a forwarded one may take.
    fn listen() -> (
        crate::process::AmbientForward,
        std::sync::mpsc::Receiver<crate::process::Ambient>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = crate::process::forward_ambient(move |ambient| {
            let _ = tx.send(ambient);
        });
        (guard, rx)
    }
    const OWED: std::time::Duration = std::time::Duration::from_secs(5);

    /// SIGINT interrupts the run in flight and leaves the durable root — and
    /// with it every detached worker — untouched.
    #[test]
    fn handler_translates_sigint_into_an_interrupt() {
        use crate::process::Ambient;
        let _serial = REQUEST_SERIAL.lock();
        clear();
        let (_guard, heard) = listen();

        handler(libc::SIGINT);
        assert_eq!(
            heard.recv_timeout(OWED),
            Ok(Ambient::Interrupt),
            "SIGINT must interrupt the run in flight"
        );
        assert!(
            heard
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "SIGINT must not reach the durable root"
        );
        assert!(
            escalation_pending(),
            "a delivered signal must tick the escalation ladder"
        );
        clear();
    }

    /// SIGTERM and SIGHUP are one shutdown request, reaching the durable root
    /// and with it every detached worker.
    #[test]
    fn handler_translates_sigterm_and_sighup_into_root_terminate() {
        use crate::process::Ambient;
        let _serial = REQUEST_SERIAL.lock();
        clear();
        clear_root_request();
        let (_guard, heard) = listen();

        handler(libc::SIGTERM);
        assert_eq!(
            heard.recv_timeout(OWED),
            Ok(Ambient::Root(CancelCause::Terminate)),
            "SIGTERM must terminate the session's durable root"
        );
        handler(libc::SIGHUP);
        assert!(
            heard
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "SIGHUP is the same shutdown request as SIGTERM, already heard"
        );
        clear();
        clear_root_request();
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
