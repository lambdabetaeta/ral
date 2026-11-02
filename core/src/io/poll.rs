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

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    use std::os::unix::thread::JoinHandleExt as _;
    use std::time::Duration;

    extern "C" fn nothing(_: libc::c_int) {}

    /// Signals aimed at the waiting thread, without `SA_RESTART`, for far
    /// longer than a slice: a fresh deadline per `EINTR` would outlast them.
    #[test]
    fn eintr_is_retried_against_the_slices_own_deadline() {
        // SIGURG is ignored by default, so no other test sees the handler.
        let action = SigAction::new(
            SigHandler::Handler(nothing),
            SaFlags::empty(),
            SigSet::empty(),
        );
        // SAFETY: the handler does nothing, async-signal-safely.
        unsafe { sigaction(Signal::SIGURG, &action) }.expect("install the handler");

        let (idle, _writer) = crate::process::cloexec_pipe().expect("pipe");
        let waiter = std::thread::spawn(move || {
            let begun = Instant::now();
            let verdict = poll_slice(&idle, PollFlags::IN);
            (verdict, begun.elapsed())
        });
        let barrage = Instant::now() + Duration::from_millis(500);
        while !waiter.is_finished() && Instant::now() < barrage {
            let thread = waiter.as_pthread_t() as libc::pthread_t;
            // SAFETY: the waiter is not joined yet, so its thread id is live;
            // it may have just exited, which `ESRCH` reports and we ignore.
            let _ = unsafe { libc::pthread_kill(thread, libc::SIGURG) };
            std::thread::sleep(Duration::from_millis(2));
        }
        let (verdict, elapsed) = waiter.join().expect("waiter thread");
        assert_eq!(verdict.expect("EINTR is not an error"), None);
        assert!(
            elapsed >= POLL_SLICE,
            "the slice ran its course: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "the barrage did not renew the deadline: {elapsed:?}"
        );
    }
}
