//! Signal handling: the platform handlers, dispositions, the escalation ladder
//! and the signals a teardown sends.
//!
//! The platform handlers translate SIGINT / SIGTERM / SIGHUP into a
//! [`CancelCause`](crate::process::CancelCause) on the scopes of
//! [`cancel`](crate::process::cancel) — SIGINT on the foreground run,
//! SIGTERM / SIGHUP on the durable root — while a separate ladder counts
//! deliveries and forces `_exit` on the third.  `unix` and `windows` exist only
//! on their own platform, so neither can be linked from this page.
//!
//! The two are exhaustive, here and throughout ral-core: `os_pipe` is an
//! unconditional dependency and builds on Unix and Windows alone, so a
//! third-platform arm is a branch no compiler ever reaches — scaffolding
//! that can only rot.

use std::sync::atomic::{AtomicU8, Ordering};

use super::cancel::CancelCause;
use super::outcome::Signal;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::gesture_signal;
#[cfg(unix)]
pub(crate) use unix::{KILL, gesture, grace_signal};
#[cfg(unix)]
pub use unix::{
    ignore, install, install_handlers, interrupt_handler, quit_handler, reset_child_signals,
    term_handler,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::gesture_signal;
#[cfg(windows)]
pub(crate) use windows::{KILL, grace_signal};
#[cfg(windows)]
pub use windows::{install_handlers, reset_child_signals};

/// Every signal a death by which is `cause`'s: its grace signal, its key, and
/// the kill that ends every teardown.
pub(crate) fn signals_of(cause: CancelCause) -> impl Iterator<Item = Signal> {
    grace_signal(cause)
        .into_iter()
        .chain(gesture_signal(cause))
        .chain([KILL])
}

// ── Escalation ladder ──────────────────────────────────────────────────────

/// Termination signals delivered since the last [`clear`]; the third forces
/// `_exit` in the platform handler.
///
/// Not a delivery mechanism — the handlers deliver through the cancel scopes,
/// which is all [`Mooring::check`](crate::types::Mooring::check) reads.  A backstop for wedged cooperative delivery
/// must not depend on what it backstops.
pub(crate) static ESCALATION: AtomicU8 = AtomicU8::new(0);

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
