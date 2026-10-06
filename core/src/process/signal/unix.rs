//! Unix signal handlers and dispositions — the platform half of `super`.
//!
//! A termination signal unwinds nothing by itself: the handler translates it
//! into an ambient cause its host forwards to the engine as `Control`, and
//! ticks an escalation ladder whose third delivery forces `_exit`.

use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

use super::ESCALATION;
use crate::process::Signal;
use crate::process::cancel::{CancelCause, request_interrupt, request_root_cancel};

// ── Termination handler ────────────────────────────────────────────────────

/// Install handlers for SIGINT, SIGTERM, SIGHUP.
///
/// [`install`] snapshots the inherited `SIG_IGN` dispositions first, since
/// afterwards ral's own are indistinguishable from the parent's.  Never name
/// SIGWINCH or SIGSEGV here: crossterm's `signal-hook-registry` owns one and
/// fff-search's crash hook may own the other, and a raw `signal(2)` install
/// unhooks it for good.
pub fn install_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        install(sig, handler);
    }
    crate::process::reaper::ensure_installed();
}

/// Ignore `sig`, snapshotting the inherited dispositions first.
pub fn ignore(sig: libc::c_int) {
    snapshot_once();
    unsafe {
        libc::signal(sig, libc::SIG_IGN);
    }
}

/// Point `sig` at `handler`, snapshotting the inherited dispositions first.
pub fn install(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    snapshot_once();
    unsafe {
        libc::signal(sig, handler as *const () as libc::sighandler_t);
    }
}

extern "C" fn handler(sig: libc::c_int) {
    let cause = handler_cause(sig);
    if ESCALATION.fetch_add(1, Ordering::Relaxed) >= 2 {
        // `_exit`, not `exit`: atexit hooks run arbitrary code under a handler.
        unsafe { libc::_exit(cause.code().into()) };
    }
    // Async-signal-safe: atomic read-modify-writes on `static`s.  SIGTERM/SIGHUP
    // land on the durable root, so detached workers hear them too.
    match cause {
        CancelCause::Interrupted => request_interrupt(),
        _ => request_root_cancel(cause),
    }
}

/// The cause [`handler`] reads `sig` as.  Pure, so async-signal-safe.
fn handler_cause(sig: libc::c_int) -> CancelCause {
    if sig == libc::SIGINT {
        CancelCause::Interrupted
    } else {
        CancelCause::Terminated
    }
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
    request_root_cancel(CancelCause::Aborted);
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

static SNAPSHOT: Once = Once::new();

/// The one snapshot, taken before the first disposition change [`ignore`] or
/// [`install`] makes.
fn snapshot_once() {
    SNAPSHOT.call_once(snapshot_inherited_ignored);
}

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
/// up, and the cause each stands for.  SIGTERM is no terminal's.
const GESTURES: [(i32, CancelCause); 3] = [
    (libc::SIGINT, CancelCause::Interrupted),
    (libc::SIGQUIT, CancelCause::Aborted),
    (libc::SIGHUP, CancelCause::Terminated),
];

/// The key `signal` is, read only by [`TerminalLoan::hear`]: nothing but a
/// terminal ral lent can report one.
pub(crate) fn gesture(signal: Signal) -> Option<CancelCause> {
    GESTURES
        .into_iter()
        .find_map(|(number, cause)| (number == signal.number()).then_some(cause))
}

/// The key a terminal sends for `cause`, if any.
pub(crate) fn gesture_signal(cause: CancelCause) -> Option<Signal> {
    GESTURES
        .into_iter()
        .find_map(|(number, key)| (key == cause).then_some(Signal::new(number)))
}

/// The signal that ends every teardown.
pub(crate) const KILL: Signal = Signal::new(libc::SIGKILL);

#[cfg(test)]
mod tests {
    use super::super::{clear, escalation_pending};
    use super::*;
    use crate::process::cancel::{REQUEST_SERIAL, clear_root_request};
    use strum::VariantArray as _;

    // ── Teardown ladder ────────────────────────────────────────────────────

    /// The catchable opening of every teardown, and the two causes that have
    /// none to offer.
    #[test]
    fn grace_signal_opens_on_the_cause() {
        for (cause, expected) in [
            (CancelCause::Interrupted, Some(libc::SIGINT)),
            (CancelCause::Cancelled, Some(libc::SIGTERM)),
            (CancelCause::TimedOut, Some(libc::SIGTERM)),
            (CancelCause::Terminated, Some(libc::SIGTERM)),
            (CancelCause::ReaderGone, None),
            (CancelCause::Aborted, None),
        ] {
            assert_eq!(
                grace_signal(cause).map(Signal::number),
                expected,
                "{cause:?} must open its teardown with {expected:?}"
            );
        }
    }

    // ── Keys ───────────────────────────────────────────────────────────────

    /// A terminal sends SIGINT, SIGQUIT and SIGHUP, and nothing else is a key.
    #[test]
    fn only_a_terminals_signals_are_keys() {
        for n in 1..=31 {
            let expected = match n {
                libc::SIGINT => Some(CancelCause::Interrupted),
                libc::SIGQUIT => Some(CancelCause::Aborted),
                libc::SIGHUP => Some(CancelCause::Terminated),
                _ => None,
            };
            assert_eq!(gesture(Signal::new(n)), expected, "signal {n}");
        }
    }

    /// `gesture_signal` names each key's signal, and no other cause has one.
    #[test]
    fn gesture_signal_inverts_gesture() {
        for &cause in CancelCause::VARIANTS {
            match gesture_signal(cause) {
                Some(signal) => assert_eq!(gesture(signal), Some(cause), "{cause:?}"),
                None => assert!(
                    GESTURES.iter().all(|&(_, key)| key != cause),
                    "{cause:?} is some key's cause"
                ),
            }
        }
    }

    /// The status a forced exit reports: the interrupt's for SIGINT, the
    /// termination's for SIGTERM and SIGHUP.
    #[test]
    fn a_forced_exit_reports_the_handlers_cause() {
        for (sig, code) in [
            (libc::SIGINT, 130),
            (libc::SIGTERM, 143),
            (libc::SIGHUP, 143),
        ] {
            assert_eq!(i32::from(handler_cause(sig).code()), code, "signal {sig}");
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
            Ok(Ambient::Root(CancelCause::Terminated)),
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
}
