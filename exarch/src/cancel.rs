//! Exarch's task-level cancel flag, layered on top of ral's SIGINT
//! handling.
//!
//! ral's `signal::install_handlers` sets `SIGNAL_COUNT` so the
//! evaluator unwinds between statements; that interrupts an in-flight
//! tool call but leaves exarch's turn loop free to keep going.  Here
//! we add a process-wide `AtomicBool` set by the same signal that
//! `run_task` polls between turns and that the HTTP request future
//! races against — so a single Ctrl-C aborts the whole task and
//! returns to the prompt.
//!
//! Install order matters: ral's handler must be set first (during
//! `boot_shell`); then `install` here replaces the disposition with a
//! handler that sets the cancel flag *and* forwards to ral's, so
//! statement-level unwinding still works.

use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);

/// True if a Ctrl-C arrived since the last `clear`.
pub fn is_set() -> bool {
    CANCEL.load(Ordering::Relaxed)
}

/// Reset the flag.  Called at the top of each readline iteration and at
/// the start of `run_task`, so a stale signal from a prior task can't
/// kill the next one before it begins.
pub fn clear() {
    CANCEL.store(false, Ordering::Relaxed);
}

/// Set the flag without going through a signal — used by the TUI's
/// Ctrl-C key handler under raw mode, where the kernel no longer
/// turns Ctrl-C into SIGINT for us.
pub fn raise() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// Install the chained signal handler.  Must run *after*
/// `ral_core::signal::install_handlers` — we capture ral's handler and
/// forward to it so its `SIGNAL_COUNT` semantics are preserved.
#[cfg(unix)]
pub fn install() {
    unsafe {
        let ral = ral_core::signal::term_handler();
        RAL_HANDLER = Some(ral);
        libc::signal(libc::SIGINT, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, chained as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
pub fn install() {}

#[cfg(unix)]
static mut RAL_HANDLER: Option<extern "C" fn(libc::c_int)> = None;

#[cfg(unix)]
extern "C" fn chained(sig: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
    // SAFETY: `RAL_HANDLER` is written once during `install` (single-
    // threaded startup) and only read here; the handler itself is a
    // plain extern "C" fn pointer with no shared state.
    unsafe {
        if let Some(h) = RAL_HANDLER {
            h(sig);
        }
    }
}
