//! `dup2` onto a process-global fd slot, for exarch's TUI stderr-to-log
//! redirect.
//!
//! The shell's own redirects never touch the fd table: fd 1/2 route through
//! `Sink`s, fd 0 through `Source`, and no other fd exists.
//!
//! Code between fork and exec does not — a `pre_exec` hook calls
//! `libc::dup2` itself, since only async-signal-safe work may run there.

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

/// Atomically retarget the open fd numbered `slot` onto whatever `src`
/// points at.
///
/// # Errors
///
/// The `dup2(2)` errno; a best-effort restore inside a `Drop` discards it.
///
/// # Safety
///
/// `slot` must name a live process-global fd slot — 0, 1 or 2.  Every
/// outstanding `BorrowedFd`/`OwnedFd` for it
/// silently starts seeing the new target, which is the point of the call,
/// not a hazard to guard against.
pub unsafe fn clobber_slot(src: BorrowedFd<'_>, slot: RawFd) -> std::io::Result<()> {
    if unsafe { libc::dup2(src.as_raw_fd(), slot) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
