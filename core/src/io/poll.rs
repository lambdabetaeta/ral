//! The one sliced wait: every blocked read or write is a sequence of (read
//! the scope; wait at most `POLL_SLICE`) steps.  Unix polls the fd; Windows,
//! which cannot poll an anonymous pipe, aborts the blocked call from the
//! thread that waits on it (`interrupt`), and the aborted call re-reads its
//! scope.

use std::io;
#[cfg(unix)]
use {
    crate::process::POLL_SLICE,
    rustix::event::{PollFd, PollFlags, Timespec, poll},
    rustix::io::Errno,
    std::os::fd::AsFd,
    std::time::Instant,
};

/// Wait for `events` on `fd` for at most `POLL_SLICE`: `None` on the slice
/// ending, else what `poll(2)` reported.  `EINTR` is retried against the
/// slice's own deadline, not a fresh one.
#[cfg(unix)]
pub(crate) fn poll_slice(fd: impl AsFd, events: PollFlags) -> io::Result<Option<PollFlags>> {
    let fd = fd.as_fd();
    let end = Instant::now() + POLL_SLICE;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let timeout = Timespec::try_from(left).expect("a slice fits a timespec");
        let mut fds = [PollFd::new(&fd, events)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) if left.is_zero() => return Ok(None),
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => return Ok(Some(fds[0].revents())),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Abort the synchronous I/O `thread` has in flight, once: a call issued
/// after this lands needs the next.
#[cfg(windows)]
pub(crate) fn interrupt<T>(thread: &std::thread::JoinHandle<T>) {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a `JoinHandle` keeps its thread's handle open for its own life.
    unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(thread.as_raw_handle().cast()) };
}

/// Whether `e` is a call [`interrupt`] aborted.  An aborted pipe read took
/// nothing; an aborted write may have delivered a prefix it cannot report.
#[cfg(windows)]
pub(crate) fn aborted(e: &io::Error) -> bool {
    e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED.cast_signed())
}
