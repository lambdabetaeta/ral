//! Windows shares its console with children; only its modes need restoring.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};

use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleMode,
};

use crate::process::Pgid;

#[derive(Debug)]
pub(super) struct Saved {
    modes: [Option<ConsoleMode>; 3],
}

#[derive(Debug)]
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

impl Saved {
    pub(super) fn capture() -> Self {
        Self {
            modes: [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
                .map(ConsoleMode::capture),
        }
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a shared console has no handoff to refuse"
)]
pub(super) fn hand_over(_leader: Pgid) -> io::Result<()> {
    Ok(())
}

impl Drop for Saved {
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
