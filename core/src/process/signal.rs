//! Signal handling: the platform handlers, dispositions, the escalation ladder
//! and the signals a teardown sends.
//!
//! The platform handlers touch no scope: each raises the ambient cause its
//! signal asks for (see [`cancel`](crate::process::cancel)), an interrupt or
//! a request on the durable root, which a host forwards to its engine as
//! `Control`; a separate ladder counts shutdown deliveries and forces `_exit`
//! on the third.  `unix` and `windows` exist only on their own platform, so
//! neither can be linked from this page.
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
pub(crate) use unix::catch;
#[cfg(unix)]
use unix::gesture_signal;
#[cfg(target_os = "linux")]
pub(crate) use unix::ignored;
#[cfg(unix)]
pub use unix::{Disposition, install, install_handlers};
#[cfg(unix)]
pub(crate) use unix::{KILL, gesture, grace_signal, reset_child_signals};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::gesture_signal;
#[cfg(windows)]
pub(crate) use windows::{KILL, grace_signal};
#[cfg(windows)]
pub use windows::{install_handlers, install_interrupt_handler};

/// Every signal a death by which is `cause`'s: its grace signal, its key, and
/// the kill that ends every teardown.
pub(crate) fn signals_of(cause: CancelCause) -> impl Iterator<Item = Signal> {
    grace_signal(cause)
        .into_iter()
        .chain(gesture_signal(cause))
        .chain([KILL])
}

// ── Escalation ladder ──────────────────────────────────────────────────────

/// Shutdown signals delivered since the last [`clear`]; the third forces
/// `_exit` in the platform handler.
///
/// Not a delivery mechanism — the handlers deliver through the ambient
/// causes a host forwards.  A backstop for wedged cooperative delivery must
/// not depend on what it backstops.
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
