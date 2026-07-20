//! The one `dup2` onto a raw process-global fd slot.
//!
//! Everywhere else in the workspace that needs to retarget fd 0/1/2 (or a
//! shell-dictated redirect fd) routes through [`clobber_slot`] rather than
//! calling `libc::dup2` directly — the shell's redirect engine
//! ([`crate::runtime::command::redirect`]) and exarch's stderr-to-log
//! guard both go through here.

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

/// Atomically retarget the open fd numbered `slot` to point at whatever
/// `src` points at — `dup2(src, slot)`.
///
/// This is the single place in the workspace allowed to `dup2` onto a raw
/// fd slot number.
///
/// # Errors
///
/// The `dup2(2)` errno — `EBADF` when `src` is closed or `slot` is out of
/// fd-table range, `EINTR`/`EBUSY` on the rare kernel paths.  Callers
/// either propagate it or discard it (a best-effort restore in a `Drop`).
///
/// # Safety
///
/// The caller asserts that `slot` is a live process-global fd slot — 0, 1,
/// 2, or an fd a shell redirect names as its target — whose atomic
/// retargeting is the intended shell semantics being implemented here.
/// Every holder of an io-safety token for `slot` (a `BorrowedFd`, an
/// `OwnedFd`) will observe the new target once this call returns; that
/// every such holder now sees the redirect is the entire point of calling
/// this function, not a hazard to guard against.
pub unsafe fn clobber_slot(src: BorrowedFd<'_>, slot: RawFd) -> std::io::Result<()> {
    if unsafe { libc::dup2(src.as_raw_fd(), slot) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
