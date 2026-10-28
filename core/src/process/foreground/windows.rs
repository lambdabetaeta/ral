//! Windows shares its console with children; only its modes need restoring.

use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};

use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleMode,
};

use crate::process::Signal;
use crate::process::cancel::CancelCause;

/// Console modes held for one foreground invocation, restored on every exit.
///
/// Capture before spawning: a child can change the shared console immediately.
/// The lease still gates access.
pub struct TerminalLoan {
    modes: [Option<ConsoleMode>; 3],
}

struct ConsoleMode {
    handle: OwnedHandle,
    mode: u32,
}

impl ConsoleMode {
    fn capture(id: u32) -> Option<Self> {
        let raw = unsafe { GetStdHandle(id) };
        if raw.is_null() || raw == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return None;
        }
        // Retain the same console object even if a standard handle is replaced.
        let handle = unsafe { BorrowedHandle::borrow_raw(raw) }
            .try_clone_to_owned()
            .ok()?;
        let mut mode = 0;
        (unsafe { GetConsoleMode(handle.as_raw_handle(), &raw mut mode) } != 0)
            .then_some(Self { handle, mode })
    }
}

/// Console state captured before spawn, awaiting the child's process group.
pub(crate) struct PreparedTerminalLoan(TerminalLoan);

impl PreparedTerminalLoan {
    pub(crate) fn lend(self, _target: i32) -> Option<TerminalLoan> {
        Some(self.0)
    }
}

impl TerminalLoan {
    pub(crate) fn prepare(
        _lease: &crate::process::TerminalLease,
        _frame: &crate::process::ScopeNode,
    ) -> Option<PreparedTerminalLoan> {
        let modes =
            [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(ConsoleMode::capture);
        modes
            .iter()
            .any(Option::is_some)
            .then_some(PreparedTerminalLoan(Self { modes }))
    }

    pub(crate) fn pressed(&self) -> Option<CancelCause> {
        None
    }

    pub(crate) fn reclaim(self, _: Option<Signal>) {}
}

impl Drop for TerminalLoan {
    fn drop(&mut self) {
        for saved in self.modes.iter().flatten() {
            if unsafe { SetConsoleMode(saved.handle.as_raw_handle(), saved.mode) } == 0 {
                crate::dbg_trace!(
                    "fg",
                    "release: SetConsoleMode failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}
