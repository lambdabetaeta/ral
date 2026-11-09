//! Private-terminal fixtures for core's Unix unit tests.
//!
//! [`isolated`] re-enters one exact libtest test, using the existing spawn
//! door. Only that fresh session's helper installs stdin or acquires a tty.
//! Neither terminal setup nor signal masking runs in the outer test process.
//! Use [`checkpoints`] for explicit readiness/release between test actors.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::Duration;

use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex as Code, Termios, tcgetattr, tcsetattr,
};

/// Bounds user-authored checkpoint messages (helper exit has its own bound).
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// Whether stdin is this session's controlling terminal or merely a tty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Controlling,
    NonControlling,
}

/// Re-enter this exact test with private PTY stdin, never the runner's tty.
/// The helper thread blocks SIGTTOU to arrange foreign foreground groups.
///
/// # Panics
/// If setup, the test body, or bounded helper exit fails.
/// A helper failure includes its captured libtest stdout/stderr.
#[track_caller]
pub(crate) fn isolated(test: &str, mode: Mode, check: impl FnOnce(&PrivatePty)) {
    super::subprocess::isolated(test, || {
        // This assertion precedes every operation that could change stdin or
        // acquire a controlling terminal. The spawn door created our session.
        // SAFETY: these calls only inspect this process's IDs.
        assert_eq!(unsafe { libc::getsid(0) }, unsafe { libc::getpid() });
        let _mask = SigttouMask::block().expect("block helper SIGTTOU");
        let pty = PrivatePty::install(mode).expect("install helper's private PTY");
        check(&pty);
    });
}

/// A master kept open through restoration, and a non-controlling open of its
/// slave. Construction is private: only the isolated helper can install one.
pub(crate) struct PrivatePty {
    // Only process exit closes the master: early close would SIGHUP the helper.
    _master: std::mem::ManuallyDrop<File>,
    slave: File,
}

impl PrivatePty {
    fn install(mode: Mode) -> io::Result<Self> {
        // O_NOCTTY is essential even in the NonControlling case: opening a
        // slave as a session leader must not implicitly acquire it.
        // SAFETY: posix_openpt has no pointer arguments.
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: posix_openpt returned a fresh descriptor.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        // SAFETY: master is an open PTY master.
        if unsafe { libc::grantpt(master.as_raw_fd()) } < 0
            || unsafe { libc::unlockpt(master.as_raw_fd()) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // ptsname's static storage is used only in this exact-test helper,
        // before the test body (and any test-authored worker) starts.
        // SAFETY: master is valid; copy the returned name before any next call.
        let name = unsafe { libc::ptsname(master.as_raw_fd()) };
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful ptsname returns a NUL-terminated name.
        let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_owned();
        // SAFETY: name is NUL-terminated and flags require no mode argument.
        let slave = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: open returned a fresh descriptor.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let pty = Self {
            _master: std::mem::ManuallyDrop::new(File::from(master)),
            slave: File::from(slave),
        };
        if mode == Mode::Controlling {
            // SAFETY: the helper is a new session leader, and this private
            // slave is not another session's controlling terminal.
            #[allow(
                clippy::useless_conversion,
                reason = "ioctl request types differ across Unix targets"
            )]
            if unsafe { libc::ioctl(pty.slave.as_raw_fd(), libc::TIOCSCTTY.into(), 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: only the helper's stdin is replaced; slave remains owned.
        if unsafe { libc::dup2(pty.slave.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // Verify both halves of the fixture's contract, rather than silently
        // using an accidentally controlling terminal for a refusal test.
        rustix::termios::tcgetattr(&pty.slave)?;
        let foreground = rustix::termios::tcgetpgrp(&pty.slave);
        match mode {
            Mode::Controlling => {
                foreground?;
            }
            Mode::NonControlling => {
                assert_eq!(foreground.unwrap_err(), rustix::io::Errno::NOTTY);
            }
        }
        Ok(pty)
    }

    /// Establish the modes changed by the PTY regressions; return the readback.
    pub(crate) fn cooked_modes(&self) -> Termios {
        let mut modes = tcgetattr(&self.slave).expect("read PTY modes");
        modes
            .local_modes
            .insert(LocalModes::ECHO | LocalModes::ICANON);
        tcsetattr(&self.slave, OptionalActions::Now, &modes).expect("set cooked PTY modes");
        tcgetattr(&self.slave).expect("read back cooked PTY modes")
    }

    /// Borrow the slave for termios snapshots, mode changes, or child stdin.
    pub(crate) fn slave(&self) -> &File {
        &self.slave
    }
}

// Termios has no PartialEq: compare public state, never padding or Debug output.
#[track_caller]
pub(crate) fn assert_modes(actual: &Termios, expected: &Termios) {
    assert_eq!(actual.input_modes, expected.input_modes);
    assert_eq!(actual.output_modes, expected.output_modes);
    assert_eq!(actual.control_modes, expected.control_modes);
    assert_eq!(actual.local_modes, expected.local_modes);
    for code in [
        Code::VINTR,
        Code::VQUIT,
        Code::VERASE,
        Code::VKILL,
        Code::VEOF,
        Code::VTIME,
        Code::VMIN,
        Code::VSTART,
        Code::VSTOP,
        Code::VSUSP,
        Code::VEOL,
        Code::VREPRINT,
        Code::VDISCARD,
        Code::VWERASE,
        Code::VLNEXT,
        Code::VEOL2,
        #[cfg(target_os = "linux")]
        Code::VSWTC,
        #[cfg(target_os = "macos")]
        Code::VDSUSP,
        #[cfg(target_os = "macos")]
        Code::VSTATUS,
    ] {
        assert_eq!(actual.special_codes[code], expected.special_codes[code]);
    }
    assert_eq!(actual.input_speed(), expected.input_speed());
    assert_eq!(actual.output_speed(), expected.output_speed());
    #[cfg(target_os = "linux")]
    assert_eq!(actual.line_discipline, expected.line_discipline);
}

/// A full-duplex, CLOEXEC pair for explicit ready/release messages between
/// test threads, or between a helper and a child given one endpoint as stdio.
///
/// # Errors
/// Returns socket creation or timeout-configuration errors.
pub(crate) fn checkpoints() -> io::Result<(Checkpoint, Checkpoint)> {
    let (a, b) = crate::process::cloexec_socketpair()?;
    Ok((Checkpoint::new(a)?, Checkpoint::new(b)?))
}

/// Bounded phase markers and shell-line releases over a private socket.
pub(crate) struct Checkpoint {
    socket: UnixStream,
}

impl Checkpoint {
    fn new(socket: UnixStream) -> io::Result<Self> {
        socket.set_read_timeout(Some(WAIT))?;
        socket.set_write_timeout(Some(WAIT))?;
        Ok(Self { socket })
    }

    /// Send a phase marker, with a bounded write.
    pub(crate) fn send(&mut self, marker: u8) -> io::Result<()> {
        self.socket.write_all(&[marker])
    }

    /// Release a shell's `read -r` with one complete line.
    pub(crate) fn release_line(&mut self) -> io::Result<()> {
        self.socket.write_all(b"G\n")
    }

    /// Await exactly the named phase; EOF and unexpected phases are errors.
    pub(crate) fn expect(&mut self, expected: u8) -> io::Result<()> {
        let mut marker = [0];
        self.socket.read_exact(&mut marker)?;
        if marker[0] != expected {
            return Err(io::Error::other(format!(
                "expected checkpoint {expected:?}, received {:?}",
                marker[0]
            )));
        }
        Ok(())
    }

    /// Give a child the full-duplex endpoint as (for example) stdout. It may
    /// write readiness to fd 1 and read release from fd 1, leaving stdin a PTY
    /// and stderr available for a captured-drain regression.
    pub(crate) fn into_stdio(self) -> Stdio {
        Stdio::from(OwnedFd::from(self))
    }
}

impl From<Checkpoint> for OwnedFd {
    fn from(checkpoint: Checkpoint) -> Self {
        Self::from(checkpoint.socket)
    }
}

struct SigttouMask(libc::sigset_t);

impl SigttouMask {
    fn block() -> io::Result<Self> {
        // SAFETY: all masks are initialized by libc before being read.
        unsafe {
            let mut mask = std::mem::zeroed();
            let mut previous = std::mem::zeroed();
            libc::sigemptyset(&raw mut mask);
            libc::sigaddset(&raw mut mask, libc::SIGTTOU);
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const mask, &raw mut previous);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(previous))
        }
    }
}

impl Drop for SigttouMask {
    fn drop(&mut self) {
        // SAFETY: the saved mask belongs to this helper's current test thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.0, std::ptr::null_mut())
        };
    }
}
