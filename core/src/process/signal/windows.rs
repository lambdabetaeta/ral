//! Windows console-event handling — counterpart to the `unix` sibling.
//!
//! [`install_handlers`] gives a bare `ral` the Unix ladder's shape: the first
//! two console events interrupt the run in flight, the third `ExitProcess`es
//! with the interrupt's status.  Nothing fans out: a group hears Ctrl-Break
//! only from the teardown of the scope that owns it.

use std::sync::atomic::Ordering;

use super::ESCALATION;
use crate::process::cancel::{CancelCause, request_interrupt};
use crate::process::outcome::Signal;

// ── Signal handler installation ────────────────────────────────────────────

pub fn install_handlers() {
    // `SetConsoleCtrlHandler` via the ctrlc crate, whose handler returns TRUE so
    // Windows leaves the process alive.  Under exarch this ladder lies dormant:
    // exarch registers later, Windows runs the newest handler first, and its
    // Ctrl-C arm claims the event before this one runs.
    let _ = ctrlc::set_handler(|| {
        if ESCALATION.fetch_add(1, Ordering::Relaxed) >= 2 {
            // `ExitProcess`, not `std::process::exit`: the handler runs on a
            // worker thread, and running CRT atexit handlers there while the main
            // thread holds a lock can deadlock — the Unix handler reaches for
            // `_exit` on the same reasoning.
            unsafe {
                windows_sys::Win32::System::Threading::ExitProcess(
                    CancelCause::Interrupted.code().into(),
                );
            }
        }
        request_interrupt();
    });
}

// ── Inherited dispositions / child-signal reset ────────────────────────────

/// No-op: Windows children inherit the parent's console-control handlers, and
/// Rust's runtime installs none we would need to undo.
pub fn reset_child_signals() {}

/// The death status a console event leaves, Ctrl-C's and Ctrl-Break's alike.
const CONSOLE_EVENT: Signal = Signal::new(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT);

/// The status ral's kill leaves.
pub(crate) const KILL: Signal = Signal::new(crate::process::outcome::KILL_EXIT_CODE);

/// Ctrl-Break, named by the death it leaves, opens every graceful teardown:
/// an owned group is the only address that takes it.
pub(crate) fn grace_signal(cause: CancelCause) -> Option<Signal> {
    match cause {
        CancelCause::Interrupted
        | CancelCause::Cancelled
        | CancelCause::TimedOut
        | CancelCause::Terminated => Some(CONSOLE_EVENT),
        CancelCause::ReaderGone | CancelCause::Aborted => None,
    }
}

/// The death status a console's Ctrl-C leaves, standing for the interrupt.
pub(crate) fn gesture_signal(cause: CancelCause) -> Option<Signal> {
    (cause == CancelCause::Interrupted).then_some(CONSOLE_EVENT)
}
