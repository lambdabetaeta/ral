use super::{Output, ral_command};
use std::time::Duration;

mod pty_helper {
    use std::io::Error;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    pub struct Pty {
        pub master: OwnedFd,
        pub slave: OwnedFd,
    }

    /// Both ends are opened before the fork; the slave name is never shared.
    pub fn open() -> Result<Pty, String> {
        let fail =
            |step, e: Error| format!("Could not create a terminal for this test ({step}): {e}");
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if fd < 0 {
            let error = Error::last_os_error();
            let hint = if error.kind() == std::io::ErrorKind::PermissionDenied {
                "; a sandbox may be blocking terminal creation (these tests need PTY device access)"
            } else {
                ""
            };
            return Err(fail("posix_openpt", error) + hint);
        }
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        let raw = master.as_raw_fd();
        if unsafe { libc::grantpt(raw) } != 0 {
            return Err(fail("grantpt", Error::last_os_error()));
        }
        if unsafe { libc::unlockpt(raw) } != 0 {
            return Err(fail("unlockpt", Error::last_os_error()));
        }
        let name =
            rustix::pty::ptsname(&master, Vec::new()).map_err(|e| fail("ptsname", e.into()))?;
        let slave = rustix::fs::open(
            name,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| fail("open slave", e.into()))?;
        Ok(Pty { master, slave })
    }

    /// Make `fd` the controlling terminal of the calling session.  Must
    /// be called from a fresh session (`setsid`) — typical post-fork
    /// pre-exec discipline for "I am the new session leader".
    ///
    /// `TIOCSCTTY` is typed differently across platforms (`Ioctl` on
    /// Linux, `c_uint` on Apple, `c_ulong` on the BSDs); the `as _`
    /// cast lets the request argument coerce to whatever `ioctl`
    /// expects on the target.  `.into()` would be `useless_conversion`
    /// on Linux, so the cast lint is the one waived.
    #[allow(clippy::cast_lossless, reason = "per-platform request type")]
    pub unsafe fn become_controlling(fd: RawFd) -> std::io::Result<()> {
        if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A live pty-backed `ral -i --norc` REPL, for tests that need more than one
/// round of input (a line, then a signal, then more input, …).
/// `run_pty_repl_until` is the single-shot case built on top of it.
pub struct PtySession {
    pub child: std::process::Child,
    pub terminal: std::fs::File,
    master: std::fs::File,
    bytes: Vec<u8>,
}

impl PtySession {
    pub fn spawn() -> Result<Self, String> {
        let mut cmd = ral_command();
        cmd.arg("-i").arg("--norc");
        cmd.env("RAL_INTERACTIVE_MODE", "minimal");
        Self::spawn_command(cmd)
    }

    pub fn spawn_command(mut cmd: std::process::Command) -> Result<Self, String> {
        use std::os::unix::process::CommandExt;

        let pty = pty_helper::open()?;
        let terminal: std::fs::File = pty.slave.into();
        let master: std::fs::File = pty.master.into();
        let flags =
            rustix::fs::fcntl_getfl(&master).map_err(|e| format!("read terminal flags: {e}"))?;
        rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| format!("make terminal nonblocking: {e}"))?;
        let stdio = || {
            terminal
                .try_clone()
                .map_err(|e| format!("duplicate terminal: {e}"))
        };
        cmd.stdin(stdio()?).stdout(stdio()?).stderr(stdio()?);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                pty_helper::become_controlling(libc::STDIN_FILENO)
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start ral under a PTY: {e}"))?;

        Ok(Self {
            child,
            terminal,
            master,
            bytes: Vec::new(),
        })
    }

    pub fn send_line(&mut self, s: &str) -> std::io::Result<()> {
        use std::io::Write;
        write!(self.master, "{s}\r")
    }

    pub fn pid(&self) -> i32 {
        self.child.id().cast_signed()
    }

    pub fn send_ctrl_c(&mut self) -> std::io::Result<()> {
        self.send_byte(3)
    }

    /// One raw byte, as a key the line discipline may act on.
    pub fn send_byte(&mut self, byte: u8) -> std::io::Result<()> {
        use std::io::Write;
        self.master.write_all(&[byte])
    }

    pub fn read_available(&mut self) {
        use std::io::Read;
        let mut chunk = [0u8; 4096];
        // A continuously writing child must not starve the caller's deadline.
        for _ in 0..64 {
            match self.master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.bytes.extend_from_slice(&chunk[..n]),
            }
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Poll until `ready`, the child exits, or `timeout` elapses, draining
    /// the pty each round so the shell never blocks on a full one.  `true`
    /// only on the `ready` case.
    pub fn poll_until(&mut self, timeout: Duration, mut ready: impl FnMut(&Self) -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            self.read_available();
            if ready(self) {
                return true;
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Poll until `done` reads true on the accumulated output, the child
    /// exits, or `timeout` elapses.  `true` only on the `done` case.
    pub fn wait_until(&mut self, timeout: Duration, done: impl Fn(&str) -> bool) -> bool {
        self.poll_until(timeout, |s| done(&s.text()))
    }

    pub fn termios(&self) -> rustix::termios::Termios {
        rustix::termios::tcgetattr(&self.master).expect("read PTY terminal modes")
    }

    /// The pty's foreground process group, read from the master this test
    /// owns — `tcgetpgrp` answers on the master side even though the caller
    /// belongs to another session.
    pub fn foreground_pgid(&self) -> Option<i32> {
        rustix::termios::tcgetpgrp(&self.master)
            .ok()
            .map(|pgid| pgid.as_raw_nonzero().get())
    }

    /// Poll until the shell hands the terminal to a pipeline group, and
    /// answer that group — the only honest "the pipeline is up" signal a
    /// test has.  An echoed command line is the *tty's* doing and says
    /// nothing about the shell, which after a fresh link is the best part of
    /// a second from its first prompt; a signal sent before the handover
    /// kills the shell outright (its handlers are not installed until it
    /// boots) or is spared as an idle interrupt at the prompt, but never
    /// reaches the pipeline.
    pub fn wait_for_lent_terminal(&mut self, timeout: Duration) -> Option<i32> {
        // `spawn` gives the shell a session of its own, so its pgid is its pid.
        let shell = self.pid();
        let mut lent = None;
        self.poll_until(timeout, |s| {
            lent = s.foreground_pgid().filter(|&pgid| pgid != shell);
            lent.is_some()
        });
        lent
    }

    pub fn wait_for_exit(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<std::process::ExitStatus> {
        let start = std::time::Instant::now();
        loop {
            self.read_available();
            if let Some(status) = self.child.try_wait()? {
                self.read_available();
                return Ok(status);
            }
            if start.elapsed() >= timeout {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "terminal child did not exit within {timeout:?}: {}",
                        self.text()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Drain during exit; on macOS, undrained output can hold even a killed child.
    pub fn finish(mut self) -> Output {
        let _ = self.child.kill();
        let status = self
            .wait_for_exit(Duration::from_secs(5))
            .expect("reap terminal child")
            .code()
            .unwrap_or(1);
        let drain_start = std::time::Instant::now();
        while drain_start.elapsed() < Duration::from_millis(200) {
            self.read_available();
            std::thread::sleep(Duration::from_millis(20));
        }
        Output {
            stdout: String::new(),
            stderr: self.text(),
            status,
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.wait_for_exit(Duration::from_secs(5));
    }
}
