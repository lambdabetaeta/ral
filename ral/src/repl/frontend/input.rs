//! A root cancellation wakes input; the session still owns its exit.

use ral_core::process::arm_callback;
use ral_core::sync::LockExt as _;
use std::io::{self, BufRead, BufReader};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

#[derive(Default)]
pub(in crate::repl) struct Input(Arc<Shutdown>);

#[derive(Default)]
pub(in crate::repl) struct Shutdown {
    ended: AtomicBool,
    reader: Mutex<Option<usize>>,
}

impl Input {
    pub(in crate::repl) fn shutdown(&self) -> Arc<Shutdown> {
        self.0.clone()
    }

    pub(super) fn ended(&self) -> bool {
        self.0.ended.load(Ordering::SeqCst)
    }

    pub(super) fn readline(&mut self) -> io::Result<Option<Reading<'_>>> {
        let mask = SignalMask::unblock_interrupt()?;
        let mut reader = self.0.reader.lock_ignore_poison();
        if self.ended() {
            return Ok(None);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "pthread_t is pointer-sized on the supported Unix targets"
        )]
        let thread = unsafe { libc::pthread_self() } as usize;
        *reader = Some(thread);
        drop(reader);
        Ok(Some(Reading {
            input: self,
            _mask: mask,
        }))
    }

    pub(super) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
        let Some(mut reading) = self.readline()? else {
            line.clear();
            return Ok(0);
        };
        // Never take type-ahead away from the next foreground command.
        let read = BufReader::with_capacity(1, &mut reading).read_line(line);
        if reading.input.ended() {
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
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
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
        // An interrupt before handler installation or the blocking read is lost.
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

impl io::Read for Reading<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, poll};
        use rustix::io::Errno;

        let stdin = rustix::stdio::stdin();
        loop {
            if buf.is_empty() || self.input.ended() {
                return Ok(0);
            }
            match rustix::io::read(stdin, &mut *buf) {
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    // Stdin may already be nonblocking; its flags belong to the caller.
                    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
                    match poll(&mut fds, None) {
                        Ok(_) | Err(Errno::INTR) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                read => return Ok(read?),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_registration_refuses_the_read() {
        let mut input = Input::default();
        input.shutdown().end();
        assert!(input.readline().unwrap().is_none());
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] signal dispositions need an isolated process"
    )]
    fn shutdown_retries_after_an_early_interrupt() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;

        if std::env::var_os("RAL_INPUT_WAKE_TEST").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "repl::frontend::input::tests::shutdown_retries_after_an_early_interrupt",
                    "--nocapture",
                ])
                .env("RAL_INPUT_WAKE_TEST", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }

        ral_core::process::install_interrupt_handler().unwrap();
        let (read, mut write) = ral_core::process::cloexec_pipe().unwrap();
        assert_eq!(
            unsafe { libc::dup2(read.as_raw_fd(), libc::STDIN_FILENO) },
            0
        );
        let mut input = Input::default();
        let shutdown = input.shutdown();
        let mut reading = input.readline().unwrap().unwrap();
        let epoch = ral_core::process::interrupt_epoch();
        shutdown.end();
        assert_ne!(ral_core::process::interrupt_epoch(), epoch);

        let (done, wait) = std::sync::mpsc::channel();
        let fallback = std::thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(2)).is_err() {
                write.write_all(b"x").unwrap();
            }
        });
        // Enter after the first interrupt, as if cancellation raced the check.
        let mut byte = [0];
        let result = rustix::io::read(rustix::stdio::stdin(), &mut byte);
        let _ = done.send(());
        fallback.join().unwrap();
        assert_eq!(result, Err(rustix::io::Errno::INTR));
        assert_eq!(reading.read(&mut byte).unwrap(), 0);
    }
}
