//! Ending a stage's blocked stdin read from another thread.
//!
//! Unix: a self-pipe the reader polls beside its own fd — a fired wake reads
//! as `POLLIN` on the wake fd, never as a byte the reader must drain.
//! Windows: anonymous pipes cannot be polled, so the reader is unblocked by
//! `CancelSynchronousIo`, and [`Wake::acknowledge`]/[`Wake::acknowledged`]
//! agree the read ended because of the wake and not some other abort.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What [`Wake::poll_beside`] saw first: the wake, or the fd's own `revents`.
#[cfg(unix)]
pub(crate) enum Readiness {
    Fired,
    Ready(i16),
}

pub struct Wake {
    fired: AtomicBool,
    #[cfg(unix)]
    read: os_pipe::PipeReader,
    #[cfg(unix)]
    write: os_pipe::PipeWriter,
    #[cfg(windows)]
    acknowledged: AtomicBool,
}

impl Wake {
    /// # Errors
    /// Unix: the self-pipe could not be created.
    // Windows makes no pipe and so cannot fail, but the `Result` is the Unix
    // arm's and every caller is written against it: dropping it here would
    // fork the signature by platform for no gain.
    #[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
    pub fn new() -> std::io::Result<Arc<Self>> {
        #[cfg(unix)]
        {
            let (read, write) = crate::process::cloexec_pipe()?;
            Ok(Arc::new(Self {
                fired: AtomicBool::new(false),
                read,
                write,
            }))
        }
        #[cfg(windows)]
        {
            Ok(Arc::new(Self {
                fired: AtomicBool::new(false),
                acknowledged: AtomicBool::new(false),
            }))
        }
    }

    /// Idempotent: only the first call touches the pipe, so a wake fired
    /// twice never blocks on a full one-byte buffer.
    // The early return guards the Unix arm's write below, which is compiled
    // out on Windows — leaving a `return` that is last only on this platform.
    #[cfg_attr(windows, allow(clippy::needless_return))]
    pub(crate) fn fire(&self) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            // `fired.swap` above guarantees a single writer; a reader
            // already gone (EPIPE) still leaves the wake reading as ready.
            let _ = (&self.write).write_all(&[0]);
        }
    }

    #[cfg(windows)]
    pub(crate) fn is_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Block until `fd` reports `events` or the wake fires; a fired wake
    /// outranks readiness that arrived beside it.  `EINTR` is retried.
    ///
    /// # Errors
    /// `poll(2)`'s own failure.
    #[cfg(unix)]
    pub(crate) fn poll_beside(
        &self,
        fd: std::os::fd::RawFd,
        events: i16,
    ) -> std::io::Result<Readiness> {
        use std::os::fd::AsRawFd;
        loop {
            let mut fds = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is two fully initialised pollfds and `2` is their count.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds[1].revents != 0 {
                return Ok(Readiness::Fired);
            }
            if fds[0].revents != 0 {
                return Ok(Readiness::Ready(fds[0].revents));
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn acknowledge(&self) {
        self.acknowledged.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    pub(crate) fn acknowledged(&self) -> bool {
        self.acknowledged.load(Ordering::SeqCst)
    }
}
