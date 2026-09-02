//! Ending a stage's blocked stdin read from another thread.
//!
//! Unix: a self-pipe the reader polls beside its own fd — a fired wake is
//! read as `POLLIN` on the wake fd, never a byte the reader must drain.
//! Windows: anonymous pipes cannot be polled, so the reader is unblocked by
//! `CancelSynchronousIo` on its own thread ([`super::signal`] callers use
//! [`Wake::acknowledge`]/[`Wake::acknowledged`] to agree the read really
//! ended because of the wake, not some other abort).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    pub fn fire(&self) {
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

    pub fn is_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    #[cfg(unix)]
    pub(crate) fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.read.as_raw_fd()
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
