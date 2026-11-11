//! Unix signal handlers and dispositions — the platform half of `super`.
//!
//! A signal unwinds nothing by itself: a handler raises the ambient cause the
//! signal asks for, which its host forwards to the engine as `Control`, and a
//! [`Disposition::Shutdown`] handler ticks the escalation ladder whose third
//! delivery forces `_exit`.  No disposition restarts a syscall it cuts: every
//! blocked call ral makes retries `EINTR` itself.

use crate::fact::FailureCode as _;
use std::io;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

use super::ESCALATION;
use crate::process::Signal;
use crate::process::cancel::{Ambient, CancelCause, RootCause, request_interrupt};

// ── Dispositions ───────────────────────────────────────────────────────────

/// What a delivered signal does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Interrupt the run in flight, whatever the signal; never escalates.
    Interrupt,
    /// Raise what the signal asks, and tick the escalation ladder.
    Shutdown,
    /// Raise what the signal asks (Ctrl-`\`: the root's abort); never
    /// escalates.
    Abort,
    /// `SIG_IGN`.
    Ignore,
    /// `SIG_DFL`.
    Default,
}

/// Give `sig` `disposition`.
///
/// The inherited dispositions are snapshotted first, since afterwards ral's
/// own are indistinguishable from the parent's.  Never name SIGWINCH:
/// crossterm's `signal-hook-registry` owns it, and an install unhooks it for
/// good.
///
/// # Errors
/// `sig` is no signal, or one that cannot be caught.
pub fn install(sig: libc::c_int, disposition: Disposition) -> io::Result<()> {
    let handler: extern "C" fn(libc::c_int) = match disposition {
        Disposition::Interrupt => interrupt,
        Disposition::Shutdown => shutdown,
        Disposition::Abort => abort,
        Disposition::Ignore => return act(sig, libc::SIG_IGN),
        Disposition::Default => return act(sig, libc::SIG_DFL),
    };
    catch(sig, handler)
}

/// Point `sig` at a handler of the caller's own, as [`install`] would.
///
/// # Errors
/// `sig` is no signal, or one that cannot be caught.
pub(crate) fn catch(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) -> io::Result<()> {
    act(sig, handler as *const () as libc::sighandler_t)
}

fn act(sig: libc::c_int, action: libc::sighandler_t) -> io::Result<()> {
    snapshot_once();
    set(sig, action)
}

/// Point `sig` at `action` without `SA_RESTART`.  Async-signal-safe.
fn set(sig: libc::c_int, action: libc::sighandler_t) -> io::Result<()> {
    // SAFETY: zeroed is a valid `sigaction`: no flags, and its mask emptied.
    let mut new: libc::sigaction = unsafe { std::mem::zeroed() };
    new.sa_sigaction = action;
    unsafe { libc::sigemptyset(&raw mut new.sa_mask) };
    if unsafe { libc::sigaction(sig, &raw const new, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// [`Disposition::Shutdown`] on SIGINT, SIGTERM and SIGHUP, with the reaper
/// armed: a batch host's whole disposition table.
///
/// # Panics
/// If one of the three cannot be caught, which no Unix allows.
pub fn install_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        install(sig, Disposition::Shutdown).expect("SIGINT, SIGTERM and SIGHUP are catchable");
    }
    crate::process::reaper::ensure_installed();
}

// ── Handlers ───────────────────────────────────────────────────────────────
//
// Async-signal-safe throughout: atomic read-modify-writes on `static`s and a
// reaper kick.

extern "C" fn interrupt(_: libc::c_int) {
    request_interrupt();
}

extern "C" fn abort(sig: libc::c_int) {
    asked(sig).raise();
}

extern "C" fn shutdown(sig: libc::c_int) {
    let asked = asked(sig);
    if ESCALATION.fetch_add(1, Ordering::Relaxed) >= 2 {
        // `_exit`, not `exit`: atexit hooks run arbitrary code under a handler.
        unsafe { libc::_exit(i32::from(asked.cause().failure_code().get())) };
    }
    asked.raise();
}

/// What a delivery of `sig` asks: its key's ambient cause, else (SIGTERM,
/// which no terminal sends) the session's termination.  Pure.
fn asked(sig: libc::c_int) -> Ambient {
    key(sig).unwrap_or(Ambient::Root(RootCause::Terminated))
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

static SNAPSHOT: Once = Once::new();

/// The one snapshot, taken before ral's first disposition change.
fn snapshot_once() {
    SNAPSHOT.call_once(snapshot_inherited_ignored);
}

fn snapshot_inherited_ignored() {
    let mask = (MANAGED_SIGNALS.iter())
        .filter(|&&sig| ignored(sig))
        .fold(0u64, |mask, &sig| mask | (1u64 << sig));
    INHERITED_IGNORED.store(mask, Ordering::Release);
}

/// Whether `sig` is ignored now.
pub(crate) fn ignored(sig: libc::c_int) -> bool {
    // SAFETY: zeroed is a valid `sigaction`, and a null action only reads.
    let mut now: libc::sigaction = unsafe { std::mem::zeroed() };
    (unsafe { libc::sigaction(sig, std::ptr::null(), &raw mut now) } == 0)
        && now.sa_sigaction == libc::SIG_IGN
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
pub(crate) fn reset_child_signals() {
    for &sig in MANAGED_SIGNALS {
        let target = if was_inherited_ignored(sig) {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        let _ = set(sig, target);
    }
    let _ = set(libc::SIGPIPE, libc::SIG_DFL);
}

/// The catchable signal a teardown opens with before its SIGKILL; `None` skips
/// straight to the kill — a reader-gone cut must not hand the verdict back to
/// the producer's own disposition, and a root abort has no grace to offer.
pub(crate) fn grace_signal(cause: CancelCause) -> Option<Signal> {
    match cause {
        CancelCause::Interrupted => Some(Signal::new(libc::SIGINT)),
        CancelCause::Cancelled | CancelCause::TimedOut | CancelCause::Terminated => {
            Some(Signal::new(libc::SIGTERM))
        }
        CancelCause::ReaderGone | CancelCause::Aborted => None,
    }
}

/// What a terminal sends when Ctrl-C or Ctrl-\ is pressed on it, or it hangs
/// up, and what each asks of the session.  SIGTERM is no terminal's.
const GESTURES: [(i32, Ambient); 3] = [
    (libc::SIGINT, Ambient::Interrupt),
    (libc::SIGQUIT, Ambient::Root(RootCause::Aborted)),
    (libc::SIGHUP, Ambient::Root(RootCause::Terminated)),
];

fn key(sig: libc::c_int) -> Option<Ambient> {
    GESTURES
        .into_iter()
        .find_map(|(number, asks)| (number == sig).then_some(asks))
}

/// The key `signal` is, read only by
/// [`TerminalHold::hear`](crate::process::TerminalHold::hear): nothing but
/// a terminal ral lent can report one.
pub(crate) fn gesture(signal: Signal) -> Option<CancelCause> {
    key(signal.number()).map(Ambient::cause)
}

/// The key a terminal sends for `cause`, if any.
pub(crate) fn gesture_signal(cause: CancelCause) -> Option<Signal> {
    GESTURES
        .into_iter()
        .find_map(|(number, asks)| (asks.cause() == cause).then_some(Signal::new(number)))
}

/// The signal that ends every teardown.
pub(crate) const KILL: Signal = Signal::new(libc::SIGKILL);

#[cfg(test)]
mod tests {
    use super::super::{clear, escalation_pending};
    use super::*;
    use crate::process::cancel::{REQUEST_SERIAL, clear_root_request};

    // ── Dispositions ───────────────────────────────────────────────────────

    /// Every disposition is installed as asked and without `SA_RESTART`, so a
    /// blocked call its handler cuts returns `EINTR`.  SIGUSR2 is nobody's in
    /// the test binary, and is left at its default.
    #[test]
    fn no_disposition_restarts_a_syscall() {
        let sig = libc::SIGUSR2;
        let current = || {
            // SAFETY: zeroed is a valid `sigaction`, and a null action only reads.
            let mut now: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(sig, std::ptr::null(), &raw mut now) },
                0
            );
            now
        };
        let handler = |f: extern "C" fn(libc::c_int)| f as *const () as libc::sighandler_t;
        for (disposition, expected) in [
            (Disposition::Interrupt, handler(interrupt)),
            (Disposition::Shutdown, handler(shutdown)),
            (Disposition::Abort, handler(abort)),
            (Disposition::Ignore, libc::SIG_IGN),
            (Disposition::Default, libc::SIG_DFL),
        ] {
            install(sig, disposition).expect("SIGUSR2 is catchable");
            let now = current();
            assert_eq!(now.sa_sigaction, expected, "{disposition:?}");
            assert_eq!(
                now.sa_flags & libc::SA_RESTART,
                0,
                "{disposition:?} restarts"
            );
        }
    }

    /// An uncatchable signal is refused, not silently left as it was.
    #[test]
    fn an_uncatchable_signal_is_refused() {
        assert!(install(libc::SIGKILL, Disposition::Shutdown).is_err());
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
    /// with it every worker — untouched.
    #[test]
    fn shutdown_translates_sigint_into_an_interrupt() {
        let _serial = REQUEST_SERIAL.lock();
        clear();
        let (_guard, heard) = listen();

        shutdown(libc::SIGINT);
        // Read at once: a concurrent run's `compile_run` clears the ladder.
        assert!(
            escalation_pending(),
            "a delivered signal must tick the escalation ladder"
        );
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
        clear();
    }

    /// SIGTERM and SIGHUP are one shutdown request, reaching the durable root
    /// and with it every worker.
    #[test]
    fn shutdown_translates_sigterm_and_sighup_into_root_terminate() {
        let _serial = REQUEST_SERIAL.lock();
        clear();
        clear_root_request();
        let (_guard, heard) = listen();

        shutdown(libc::SIGTERM);
        assert_eq!(
            heard.recv_timeout(OWED),
            Ok(Ambient::Root(RootCause::Terminated)),
            "SIGTERM must terminate the session's durable root"
        );
        shutdown(libc::SIGHUP);
        assert!(
            heard
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "SIGHUP is the same shutdown request as SIGTERM, already heard"
        );
        clear();
        clear_root_request();
    }

    /// The calm handlers never tick the ladder: an interrupt reaches the run
    /// in flight, Ctrl-`\` the durable root with its own cause.
    #[test]
    fn interrupt_and_abort_never_escalate() {
        let _serial = REQUEST_SERIAL.lock();
        clear();
        clear_root_request();
        let (_guard, heard) = listen();

        interrupt(libc::SIGINT);
        assert_eq!(heard.recv_timeout(OWED), Ok(Ambient::Interrupt));
        abort(libc::SIGQUIT);
        assert_eq!(
            heard.recv_timeout(OWED),
            Ok(Ambient::Root(RootCause::Aborted)),
            "Ctrl-\\ must abort the session's durable root"
        );
        assert!(
            !escalation_pending(),
            "neither may tick the escalation ladder"
        );
        clear_root_request();
    }
}
