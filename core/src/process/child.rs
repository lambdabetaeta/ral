//! A spawned child, and the one way from it to the reaper.

use super::outcome::WaitOutcome;
use super::reaper::{Watch, watch};
use crate::process::site::Silent;
use crate::site::site;

// ── Child handle ───────────────────────────────────────────────────────────
//
// Bare `Child::wait` / `try_wait` omit `WUNTRACED`: a SIGSTOP'd child reads as
// "still running" and `wait` blocks forever.  `clippy.toml` disallows the raw
// pair outside `ChildHandle`; the few exceptions are typed sites.

/// A spawned child.
///
/// [`Self::into_watch`] is the way to the reaper, which owns the wait on
/// both platforms; [`Self::reap`] is the blocking wait after a confirmed
/// kill.
pub struct ChildHandle {
    repr: ChildRepr,
    /// What a Linux supervisor will say of its program's end.
    #[cfg(target_os = "linux")]
    testimony: Option<super::report::Testimony>,
    #[cfg(target_os = "linux")]
    envelope_stderr: Option<std::fs::File>,
}

enum ChildRepr {
    #[cfg_attr(not(unix), allow(dead_code))]
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(crate::process::launch::RawChild),
}

impl ChildHandle {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn from_std(child: std::process::Child) -> Self {
        Self {
            repr: ChildRepr::Std(child),
            #[cfg(target_os = "linux")]
            testimony: None,
            #[cfg(target_os = "linux")]
            envelope_stderr: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_windows_raw(child: crate::process::launch::RawChild) -> Self {
        Self {
            repr: ChildRepr::RawWindows(child),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn testify(&mut self, testimony: super::report::Testimony) {
        self.testimony = Some(testimony);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn envelope_stderr(&mut self, file: Option<std::fs::File>) {
        self.envelope_stderr = file;
    }

    /// What the dead launcher left on its diagnostic file or piped stderr,
    /// never waiting for more; `None` where neither exists or it said nothing.
    #[cfg(unix)]
    pub(crate) fn stderr_said(&mut self) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(file) = self.envelope_stderr.as_ref() {
            return file_said(file);
        }
        let ChildRepr::Std(child) = &mut self.repr;
        let stderr = child.stderr.take()?;
        pipe_said(stderr)
    }

    pub fn id(&self) -> u32 {
        match &self.repr {
            ChildRepr::Std(child) => child.id(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.id(),
        }
    }

    /// # Errors
    /// Returns `Err` if the platform kill — SIGKILL on Unix,
    /// `TerminateProcess` on Windows — fails.
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.repr {
            ChildRepr::Std(child) => child.kill(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.kill(),
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.repr {
            ChildRepr::Std(child) => child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stdout(),
        }
    }

    #[cfg(test)]
    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.repr {
            ChildRepr::Std(child) => child
                .stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stderr(),
        }
    }

    #[cfg(windows)]
    pub(crate) fn raw_process_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        match &self.repr {
            ChildRepr::Std(child) => {
                use std::os::windows::io::AsRawHandle;
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            }
            ChildRepr::RawWindows(child) => child.raw_process_handle(),
        }
    }

    /// Blocking reap after a confirmed SIGKILL, which terminates even a stopped
    /// process — so the one raw `Child::wait` inside `ChildHandle` lives here.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    pub(crate) fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.repr {
            ChildRepr::Std(child) => site!(Silent::ChildReap, child => wait()),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }

    /// The one way from a spawned child to a [`Watch`]: consumes the
    /// handle, so no later `wait` on this pid is writable behind the
    /// reaper's back.  The stdio ends must already be taken.  On Linux the
    /// outcome every subscriber sees is the supervisor's word on its program.
    pub(crate) fn into_watch<E: Send + 'static>(
        self,
        tx: std::sync::mpsc::Sender<E>,
        f: impl FnOnce(WaitOutcome) -> E + Send + 'static,
    ) -> Watch {
        let pid = self.id();
        #[cfg(target_os = "linux")]
        let f = {
            let testimony = self.testimony;
            move |outcome| f(testimony.map_or(outcome, |testimony| testimony.amend(outcome)))
        };
        let watch = watch(pid, tx, f);
        // Dropping a `std::process::Child` neither kills nor reaps on Unix,
        // and on Windows only closes std's own handle — the watch opened its
        // own while registering.
        drop(self.repr);
        watch
    }
}

/// Read a dead launcher's diagnostics without waiting for a descendant.
#[cfg(unix)]
fn pipe_said(pipe: impl std::io::Read + std::os::fd::AsFd) -> Option<String> {
    use std::io::Read;
    rustix::fs::fcntl_setfl(&pipe, rustix::fs::OFlags::NONBLOCK).ok()?;
    let mut said = Vec::new();
    let _ = pipe.take(4096).read_to_end(&mut said);
    let said = String::from_utf8_lossy(&said).trim().to_owned();
    (!said.is_empty()).then_some(said)
}

/// Startup diagnostics from an envelope's anonymous file. `read_at` leaves
/// the shared writer offset alone, even while a descendant is still writing.
#[cfg(target_os = "linux")]
pub(super) fn file_said(file: &std::fs::File) -> Option<String> {
    use std::os::unix::fs::FileExt;
    let mut bytes = [0; 4096];
    let count = loop {
        match file.read_at(&mut bytes, 0) {
            Ok(count) => break count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    };
    let said = String::from_utf8_lossy(&bytes[..count]).trim().to_owned();
    (!said.is_empty()).then_some(said)
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    #[test]
    fn envelope_diagnostics_read_from_zero_without_moving_the_writer() {
        use std::io::{Seek, Write};
        let mut writer = std::fs::File::from(
            rustix::fs::memfd_create("diagnostics-test", rustix::fs::MemfdFlags::CLOEXEC)
                .expect("diagnostic memfd"),
        );
        writer.write_all(b"REFUSED").unwrap();
        let reader = writer.try_clone().unwrap();
        assert_eq!(super::file_said(&reader).as_deref(), Some("REFUSED"));
        assert_eq!(writer.stream_position().unwrap(), 7);
        writer.write_all(&[b'x'; 4096]).unwrap();
        assert_eq!(super::file_said(&reader).unwrap().len(), 4096);
        assert_eq!(writer.stream_position().unwrap(), 4103);
        // Reading/dropping the parent's owner does not break later writes.
        drop(reader);
        writer.write_all(b"still writable").unwrap();
    }
}
