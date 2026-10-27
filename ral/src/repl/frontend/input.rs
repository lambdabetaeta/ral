//! A root cancellation wakes input; the session still owns its exit.

use ral_core::process::{arm_callback, interrupt_epoch};
use ral_core::sync::LockExt as _;
use std::io;
use std::marker::PhantomData;
use std::os::fd::{AsRawFd as _, OwnedFd};
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

    pub(super) fn read_line(&mut self, line: &mut String, epoch: u64) -> io::Result<usize> {
        let Some(reading) = self.readline()? else {
            line.clear();
            return Ok(0);
        };
        let mut reading = PromptReader::new(reading, epoch)?;
        // BufRead::read_line retries Interrupted; a prompt must not.
        let read = read_one_line(&mut reading, line);
        if reading.reading.input.ended() {
            line.clear();
            Ok(0)
        } else if interrupt_epoch() != epoch {
            line.clear();
            Err(io::ErrorKind::Interrupted.into())
        } else {
            read
        }
    }
}

// One byte at a time: the next command owns every byte after this LF.
fn read_one_line(reader: &mut impl io::Read, line: &mut String) -> io::Result<usize> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while bytes.last() != Some(&b'\n') && reader.read(&mut byte)? != 0 {
        bytes.push(byte[0]);
    }
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("prompt input is not valid UTF-8: {e}"),
        )
    })?;
    line.push_str(text);
    Ok(bytes.len())
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
        // Rustyline may not have installed its handler yet.
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

struct PromptReader<'a> {
    reading: Reading<'a>,
    epoch: u64,
    wake: OwnedFd,
    _forward: ral_core::process::AmbientForward,
    /// Runs before each wait: where a test races a signal against it.
    #[cfg(test)]
    before_wait: Option<Box<dyn FnMut()>>,
}

impl<'a> PromptReader<'a> {
    fn new(reading: Reading<'a>, epoch: u64) -> io::Result<Self> {
        let (wake, write) = ral_core::process::cloexec_pipe()?;
        let (wake, write): (OwnedFd, OwnedFd) = (wake.into(), write.into());
        if !usize::try_from(wake.as_raw_fd()).is_ok_and(|fd| fd < libc::FD_SETSIZE) {
            return Err(io::Error::other(format!(
                "cannot wait for prompt input: wake descriptor {} exceeds pselect's descriptor limit ({})",
                wake.as_raw_fd(),
                libc::FD_SETSIZE,
            )));
        }
        for fd in [&wake, &write] {
            let flags = rustix::fs::fcntl_getfl(fd)?;
            rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        }
        // SIGINT may run on another thread; its epoch alone cannot wake pselect.
        let forward = ral_core::process::forward_ambient(move |_| {
            while rustix::io::write(&write, &[1]) == Err(rustix::io::Errno::INTR) {}
        });
        Ok(Self {
            reading,
            epoch,
            wake,
            _forward: forward,
            #[cfg(test)]
            before_wait: None,
        })
    }
}

impl io::Read for PromptReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use rustix::io::Errno;

        let mask = SignalMask::block_interrupt()?;
        let mut waiting_mask = mask.0;
        unsafe { libc::sigdelset(&raw mut waiting_mask, libc::SIGINT) };
        loop {
            if buf.is_empty() || self.reading.input.ended() {
                return Ok(0);
            }
            if interrupt_epoch() != self.epoch {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut fds = unsafe { std::mem::zeroed() };
            unsafe {
                libc::FD_ZERO(&raw mut fds);
                libc::FD_SET(libc::STDIN_FILENO, &raw mut fds);
                libc::FD_SET(self.wake.as_raw_fd(), &raw mut fds);
            }
            #[cfg(test)]
            if let Some(before_wait) = &mut self.before_wait {
                before_wait();
            }
            let ready = unsafe {
                libc::pselect(
                    self.wake.as_raw_fd().max(libc::STDIN_FILENO) + 1,
                    &raw mut fds,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &raw const waiting_mask,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if self.reading.input.ended() {
                return Ok(0);
            }
            if interrupt_epoch() != self.epoch {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if unsafe { libc::FD_ISSET(self.wake.as_raw_fd(), &raw const fds) } {
                let _ = rustix::io::read(&self.wake, &mut [0; 64]);
                continue;
            }
            // Readiness and the unique reader permit one byte without changing stdin's flags.
            match rustix::io::read(rustix::stdio::stdin(), &mut buf[..1]) {
                Err(Errno::INTR | Errno::AGAIN) => {}
                read => return Ok(read?),
            }
        }
    }
}

struct SignalMask(libc::sigset_t, PhantomData<*const ()>);

impl SignalMask {
    fn unblock_interrupt() -> io::Result<Self> {
        Self::change_interrupt(libc::SIG_UNBLOCK)
    }

    fn block_interrupt() -> io::Result<Self> {
        Self::change_interrupt(libc::SIG_BLOCK)
    }

    fn change_interrupt(how: libc::c_int) -> io::Result<Self> {
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            let mut old = std::mem::MaybeUninit::uninit();
            let error = libc::pthread_sigmask(how, &raw const set, old.as_mut_ptr());
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn line_assembly_leaves_typeahead_and_accepts_final_unterminated_utf8() {
        let mut source = "first\n界".as_bytes();
        let mut line = String::new();
        assert_eq!(read_one_line(&mut source, &mut line).unwrap(), 6);
        assert_eq!(line, "first\n");
        assert_eq!(source, "界".as_bytes());
        line.clear();
        assert_eq!(read_one_line(&mut source, &mut line).unwrap(), 3);
        assert_eq!(line, "界");
        line.clear();
        assert_eq!(read_one_line(&mut source, &mut line).unwrap(), 0);
        assert_eq!(line, "");
    }

    #[test]
    fn invalid_utf8_does_not_enter_the_string() {
        let mut line = "prefix".to_owned();
        let error = read_one_line(&mut b"bad\xff\nnext".as_slice(), &mut line).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(line, "prefix");
    }

    #[test]
    fn interruption_discards_partial_bytes_without_retrying() {
        struct Interrupted(bool);
        impl io::Read for Interrupted {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    buf[0] = b'x';
                    Ok(1)
                }
            }
        }
        let mut line = String::new();
        assert_eq!(
            read_one_line(&mut Interrupted(false), &mut line)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(line, "");
    }

    #[test]
    fn interrupt_before_read_entry_does_not_block_or_consume_input() {
        if std::env::var_os("RAL_INPUT_EPOCH_TEST").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "repl::frontend::input::tests::interrupt_before_read_entry_does_not_block_or_consume_input"])
                .env("RAL_INPUT_EPOCH_TEST", "1")
                .output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        ral_core::process::install_interrupt_handler().unwrap();
        let (read, mut write) = ral_core::process::cloexec_pipe().unwrap();
        assert_eq!(unsafe { libc::dup2(read.as_raw_fd(), 0) }, 0);
        let epoch = interrupt_epoch();
        let mut input = Input::default();
        let reading = input.readline().unwrap().unwrap();
        let mut reading = PromptReader::new(reading, epoch).unwrap();
        ral_core::process::request_interrupt();
        write.write_all(b"next\n").unwrap();
        assert_eq!(
            reading.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        drop(reading);
        let mut line = String::new();
        assert_eq!(
            input.read_line(&mut line, epoch).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(line, "");
        assert_eq!(input.read_line(&mut line, interrupt_epoch()).unwrap(), 5);
        assert_eq!(line, "next\n");
    }

    #[test]
    fn interrupt_between_epoch_check_and_wait_is_not_lost() {
        if std::env::var_os("RAL_INPUT_WAIT_TEST").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "repl::frontend::input::tests::interrupt_between_epoch_check_and_wait_is_not_lost"])
                .env("RAL_INPUT_WAIT_TEST", "1")
                .output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        use std::io::{Read as _, Write as _};
        ral_core::process::install_interrupt_handler().unwrap();
        for other_thread in [false, true] {
            for nonblocking in [false, true] {
                let (read, mut write) = ral_core::process::cloexec_pipe().unwrap();
                if nonblocking {
                    rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK).unwrap();
                }
                assert_eq!(unsafe { libc::dup2(read.as_raw_fd(), 0) }, 0);
                let mut input = Input::default();
                let epoch = interrupt_epoch();
                let reading = input.readline().unwrap().unwrap();
                let mut reading = PromptReader::new(reading, epoch).unwrap();
                let (done, wait) = std::sync::mpsc::channel();
                let fallback = std::thread::spawn(move || {
                    if wait.recv_timeout(Duration::from_secs(2)).is_err() {
                        write.write_all(b"fallback\n").unwrap();
                    }
                    write
                });
                let mut injected = false;
                reading.before_wait = Some(Box::new(move || {
                    if std::mem::replace(&mut injected, true) {
                        return;
                    }
                    if other_thread {
                        std::thread::spawn(|| {
                            let _mask = SignalMask::unblock_interrupt().unwrap();
                            assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
                        })
                        .join()
                        .unwrap();
                        assert_ne!(interrupt_epoch(), epoch);
                    } else {
                        // SIGINT is pending on this pthread until pselect unmasks it.
                        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
                        assert_eq!(interrupt_epoch(), epoch);
                    }
                }));
                let result = reading.read(&mut [0]);
                let _ = done.send(());
                let mut write = fallback.join().unwrap();
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
                drop(reading);
                write.write_all(b"next\n").unwrap();
                let mut line = String::new();
                assert_eq!(input.read_line(&mut line, interrupt_epoch()).unwrap(), 5);
                assert_eq!(line, "next\n");
            }
        }
    }

    #[test]
    fn cancellation_before_registration_refuses_the_read() {
        let mut input = Input::default();
        input.shutdown().end();
        assert!(input.readline().unwrap().is_none());
    }

    #[test]
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
        let reading = input.readline().unwrap().unwrap();
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
        let mut reading = PromptReader::new(reading, interrupt_epoch()).unwrap();
        assert_eq!(reading.read(&mut byte).unwrap(), 0);
    }
}
