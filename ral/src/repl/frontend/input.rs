//! A root cancellation wakes input; the session still owns its exit.

use ral_core::io::Wake;
use ral_core::process::arm_callback;
use ral_core::sync::LockExt as _;
use std::io::{self, BufRead, BufReader};
use std::marker::PhantomData;
use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

pub(in crate::repl) struct Input(Arc<Shutdown>);

pub(in crate::repl) struct Shutdown {
    wake: Arc<Wake>,
    reader: Mutex<Option<usize>>,
}

impl Input {
    pub(in crate::repl) fn new() -> io::Result<Self> {
        Ok(Self(Arc::new(Shutdown {
            wake: Wake::new()?,
            reader: Mutex::default(),
        })))
    }

    pub(in crate::repl) fn shutdown(&self) -> Arc<Shutdown> {
        self.0.clone()
    }

    pub(super) fn ended(&self) -> bool {
        self.0.wake.is_fired()
    }

    pub(super) fn readline(&mut self) -> io::Result<Option<Reading<'_>>> {
        let mask = SignalMask::unblock_interrupt()?;
        let mut reader = self.0.reader.lock_ignore_poison();
        if self.ended() {
            return Ok(None);
        }
        *reader = Some(unsafe { libc::pthread_self() } as usize);
        drop(reader);
        Ok(Some(Reading {
            input: self,
            _mask: mask,
        }))
    }

    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "one reader owns stdin's mode until this call returns"
    )]
    pub(super) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
        let stdin = rustix::stdio::stdin();
        let _mode = Nonblocking::new(stdin)?;
        // Never take type-ahead away from the next foreground command.
        let read = BufReader::with_capacity(
            1,
            Stdin {
                fd: stdin,
                wake: &self.0.wake,
            },
        )
        .read_line(line);
        if self.ended() {
            line.clear();
            Ok(0)
        } else {
            read
        }
    }
}

impl Shutdown {
    pub(in crate::repl) fn end(self: &Arc<Self>) {
        let reader = self.reader.lock_ignore_poison();
        if self.wake.is_fired() {
            return;
        }
        self.wake.fire();
        drop(reader);
        self.poke();
    }

    fn poke(self: &Arc<Self>) {
        {
            let reader = match self.reader.try_lock() {
                Ok(reader) => Some(reader),
                Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(reader) = reader {
                let Some(thread) = *reader else { return };
                // Registration holds the pthread alive through delivery.
                unsafe { libc::pthread_kill(thread as libc::pthread_t, libc::SIGINT) };
            }
        }
        // Rustyline installs its handler inside readline; an early poke is lost.
        let shutdown = self.clone();
        arm_callback(Duration::from_millis(20), move || shutdown.poke()).keep();
    }
}

#[must_use]
pub(super) struct Reading<'a> {
    input: &'a mut Input,
    _mask: SignalMask,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        *self.input.0.reader.lock_ignore_poison() = None;
    }
}

struct SignalMask(libc::sigset_t, PhantomData<*const ()>);

impl SignalMask {
    fn unblock_interrupt() -> io::Result<Self> {
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            let mut old = std::mem::MaybeUninit::uninit();
            let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, old.as_mut_ptr());
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(old.assume_init(), PhantomData))
        }
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.0, std::ptr::null_mut())
        };
    }
}

struct Nonblocking<'a>(BorrowedFd<'a>, rustix::fs::OFlags);

impl<'a> Nonblocking<'a> {
    fn new(fd: BorrowedFd<'a>) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(fd)?;
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self(fd, flags))
    }
}

impl Drop for Nonblocking<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::fcntl_setfl(self.0, self.1);
    }
}

struct Stdin<'a> {
    fd: BorrowedFd<'a>,
    wake: &'a Wake,
}

impl io::Read for Stdin<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.wake.wait_readable(self.fd)? {
                return Ok(0);
            }
            match rustix::io::read(self.fd, &mut *buf) {
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                read => return Ok(read?),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd as _;

    #[test]
    fn cancellation_before_registration_refuses_the_read() {
        let mut input = Input::new().unwrap();
        input.shutdown().end();
        assert!(input.readline().unwrap().is_none());
    }

    #[test]
    fn stale_readiness_cannot_block_a_read() {
        use std::io::{Read as _, Write as _};
        let (read, mut write) = ral_core::process::cloexec_pipe().unwrap();
        let wake = Wake::new().unwrap();
        let flags = rustix::fs::fcntl_getfl(&read).unwrap();
        {
            let _mode = Nonblocking::new(read.as_fd()).unwrap();
            write.write_all(b"x").unwrap();
            assert!(wake.wait_readable(&read).unwrap());
            let mut byte = [0];
            rustix::io::read(&read, &mut byte).unwrap();
            assert_eq!(
                rustix::io::read(&read, &mut byte),
                Err(rustix::io::Errno::AGAIN)
            );
            wake.fire();
            assert_eq!(
                Stdin {
                    fd: read.as_fd(),
                    wake: &wake
                }
                .read(&mut byte)
                .unwrap(),
                0
            );
        }
        assert_eq!(rustix::fs::fcntl_getfl(&read).unwrap(), flags);
    }
}
