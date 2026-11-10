//! An envelope's mandatory resources and payload placement.

#[cfg(test)]
use super::StdioSpec;
use super::{Launch, Launcher, PayloadPgid, Spawned, Unstarted};
use crate::process::group::Step;
use crate::process::report::Report;
use crate::process::{Group, Placement, Scope, TerminalHold};
use std::process::Command;

/// Reserved payload slots and independent envelope stdio, allocated before
/// spawning, never over an exec-error pipe.
pub(crate) struct EnvelopeStdio {
    pub(crate) payload: [std::os::fd::OwnedFd; 3],
    pub(crate) null: std::os::fd::OwnedFd,
    pub(crate) error: std::os::fd::OwnedFd,
    pub(crate) diagnostics: std::fs::File,
}

pub(super) struct EnvelopeStreams {
    payload: [std::os::fd::OwnedFd; 3],
    null: std::os::fd::OwnedFd,
    error: std::os::fd::OwnedFd,
}

impl EnvelopeStreams {
    /// Save the final payload streams, then give the envelope its endpoints.
    ///
    /// # Safety
    /// Only between fork and exec: replaces this child's fds and reserved slots.
    pub(super) unsafe fn capture(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: targets are reserved slots or standard streams, never an
        // exec-error pipe. Only async-signal-safe syscalls follow.
        unsafe {
            for (source, target) in [0, 1, 2].into_iter().zip(&self.payload) {
                if libc::dup2(source, target.as_raw_fd()) < 0 {
                    // A closed stream must not leave a slot bwrap could reuse.
                    return Err(std::io::Error::last_os_error());
                }
            }
            // bwrap discovers the shared tty from stdout for /dev/console.
            let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
            let tty_stdout = libc::ioctl(1, libc::TCGETS, termios.as_mut_ptr()) == 0;
            let sources = [&self.null, &self.null, &self.error];
            for (target, source) in [0, 1, 2].into_iter().zip(sources) {
                if target == 1 && tty_stdout {
                    continue;
                }
                if libc::dup2(source.as_raw_fd(), target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }
    }
}

/// A launch run through an envelope, a separate binary that runs the target
/// in a session of its own. Its payload leads that session, whatever placement
/// is asked.
pub(crate) struct EnvelopedLaunch {
    launch: Launch,
    /// `None` where the envelope reports no payload group.
    payload_pgid: Option<PayloadPgid>,
}

impl EnvelopedLaunch {
    /// `program` names the envelope in a failed start.
    pub(crate) fn new(
        cmd: Command,
        stdio: EnvelopeStdio,
        program: &'static str,
        payload_pgid: Option<PayloadPgid>,
        report: Report,
    ) -> Self {
        let EnvelopeStdio {
            payload,
            null,
            error,
            diagnostics,
        } = stdio;
        let mut enveloped = Self::unslotted(cmd, program, payload_pgid, report, diagnostics);
        enveloped.launch.setup.streams = Some(EnvelopeStreams {
            payload,
            null,
            error,
        });
        enveloped
    }

    /// A stand-in keeping the command's stdio, without payload slots.
    #[cfg(test)]
    pub(crate) fn synthetic(
        cmd: Command,
        program: &'static str,
        payload_pgid: Option<PayloadPgid>,
        report: Report,
    ) -> Self {
        let diagnostics = tempfile::tempfile().expect("empty envelope diagnostics");
        Self::unslotted(cmd, program, payload_pgid, report, diagnostics)
    }

    fn unslotted(
        cmd: Command,
        program: &'static str,
        payload_pgid: Option<PayloadPgid>,
        report: Report,
        diagnostics: std::fs::File,
    ) -> Self {
        let mut launch = Launch::from_command(cmd);
        launch.launcher = Some(Launcher {
            name: program,
            report,
            diagnostics: Some(diagnostics),
        });
        Self {
            launch,
            payload_pgid,
        }
    }

    pub(crate) fn launch_mut(&mut self) -> &mut Launch {
        &mut self.launch
    }

    /// Land on the payload's group, or the envelope's own where it named none.
    #[allow(
        clippy::result_large_err,
        reason = "the request retains captured terminal state until the failed child is reaped"
    )]
    pub(crate) fn spawn(
        self,
        placement: Placement,
        scope: &Scope,
    ) -> Result<Spawned<Group>, Unstarted> {
        let Self {
            launch,
            payload_pgid,
        } = self;
        launch.born(
            Step::NewLeader,
            placement,
            scope,
            payload_pgid,
            |placement, monitor, payload| {
                Ok((
                    Group::Enveloped(payload.unwrap_or(monitor)),
                    placement.into_guard(),
                ))
            },
        )
    }

    pub(crate) fn spawn_detached(
        self,
        scope: &Scope,
    ) -> Result<crate::process::group::Survivor, Unstarted> {
        self.launch.spawn_detached(scope)
    }

    #[cfg(test)]
    pub(crate) fn output(mut self) -> Result<std::process::Output, Unstarted> {
        self.launch_mut()
            .stdout(StdioSpec::Piped)
            .stderr(StdioSpec::Piped);
        let unstopped = crate::process::DurableRoot::default().child();
        Ok(super::output(self.spawn(Placement::Inherit, &unstopped)?))
    }
}

impl Placement {
    fn into_guard(self) -> TerminalHold {
        match self {
            Self::Tenant(prepared) => prepared.guard(),
            _ => TerminalHold::default(),
        }
    }
}

/// Startup diagnostics from an envelope's anonymous file. `read_at` leaves
/// the shared writer offset alone, even while a descendant is still writing.
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

#[cfg(test)]
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
