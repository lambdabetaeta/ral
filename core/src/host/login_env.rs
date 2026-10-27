//! The POSIX login helper's environment report, written and read here.

use super::site::Silent;
use crate::authority::sandbox::reexec;
use crate::site::site;
use nix::sys::signal::{self, SigAction, SigHandler, Signal};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;

const REPORT_FD: RawFd = 3;

pub(crate) fn serve_report() -> u8 {
    // SAFETY: REPORT_FD is borrowed; ManuallyDrop keeps it open.
    let mut report = ManuallyDrop::new(unsafe { File::from_raw_fd(REPORT_FD) });
    match report.write_all(&encode(super::vars_os())) {
        Ok(()) => 0,
        Err(e) => {
            crate::errln!("ral: cannot write the login environment report: {e}");
            1
        }
    }
}

fn encode(env: Vec<(OsString, OsString)>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (key, value) in env {
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(b'=');
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    bytes
}

fn decode(bytes: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let records = bytes.strip_suffix(b"\0")?.split(|&b| b == 0);
    let mut env = Vec::new();
    let mut records = records.peekable();
    while let Some(record) = records.next() {
        if record.is_empty() {
            return records.peek().is_none().then_some(env);
        }
        let equals = record.iter().position(|&b| b == b'=')?;
        if equals == 0 {
            return None;
        }
        env.push((
            OsString::from_vec(record[..equals].to_vec()),
            OsString::from_vec(record[equals + 1..].to_vec()),
        ));
    }
    None
}

/// Collect the environment reported after `/bin/sh -l` reads the profiles.
///
/// # Errors
/// The helper failed, or did not leave a complete environment report.
pub fn collect() -> Result<Vec<(OsString, OsString)>, String> {
    let mut report =
        tempfile::tempfile().map_err(|e| format!("cannot create the environment report: {e}"))?;
    let (exe, flag) = reexec::indirect(crate::Role::EnvReport)
        .map_err(|e| format!("cannot locate the environment reporter: {e}"))?;
    let mut command = site!(Silent::LoginCommand, Command::new("/bin/sh"));
    command
        .args(["-l", "-c", r#"exec "$0" "$1""#])
        .arg(exe)
        .arg(flag);
    let report_fd = report.as_raw_fd();
    // SAFETY: the child calls only async-signal-safe descriptor/signal operations.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(report_fd, REPORT_FD) == -1 {
                return Err(io::Error::last_os_error());
            }
            // dup2 leaves CLOEXEC intact when both descriptors are equal.
            if libc::fcntl(REPORT_FD, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for signal in [Signal::SIGINT, Signal::SIGQUIT] {
                signal::signal(signal, SigHandler::SigDfl)?;
            }
            Ok(())
        });
    }
    let status = {
        let _interrupt = Ignored::new(Signal::SIGINT)?;
        let _quit = Ignored::new(Signal::SIGQUIT)?;
        let mut child = site!(Silent::LoginSpawn, &mut command => spawn())
            .map_err(|e| format!("/bin/sh could not be started: {e}"))?;
        site!(Silent::LoginWait, &mut child => wait())
            .map_err(|e| format!("cannot wait for /bin/sh: {e}"))?
    };
    if let Some(signal) = status.signal() {
        return Err(format!("/bin/sh was killed by signal {signal}"));
    }
    if !status.success() {
        return Err(format!(
            "/bin/sh exited with status {}",
            status.code().unwrap_or(1)
        ));
    }
    report
        .rewind()
        .map_err(|e| format!("cannot rewind the environment report: {e}"))?;
    let mut bytes = Vec::new();
    report
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read the environment report: {e}"))?;
    if bytes.is_empty() {
        return Err("/bin/sh finished without reporting an environment; does ~/.profile exec another program? Start it from the rc's startup: block instead".into());
    }
    decode(&bytes).ok_or_else(|| "the environment report was malformed".into())
}

struct Ignored(Signal, SigAction);

impl Ignored {
    fn new(signal: Signal) -> Result<Self, String> {
        let ignore = SigAction::new(
            SigHandler::SigIgn,
            signal::SaFlags::empty(),
            signal::SigSet::empty(),
        );
        // SAFETY: no handler is installed; the prior disposition is restored on drop.
        let previous = unsafe { signal::sigaction(signal, &ignore) }.map_err(|e| {
            format!("cannot ignore {signal} while reading the login environment: {e}")
        })?;
        Ok(Self(signal, previous))
    }
}

impl Drop for Ignored {
    fn drop(&mut self) {
        // SAFETY: restore the disposition saved for this signal.
        let _ = unsafe { signal::sigaction(self.0, &self.1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_preserves_values_as_os_bytes() {
        let env = vec![
            ("A".into(), OsString::from_vec(b"a=b\n\xff".to_vec())),
            ("B".into(), OsString::new()),
        ];
        assert_eq!(encode(env.clone()), b"A=a=b\n\xff\0B=\0\0");
        assert_eq!(decode(&encode(env.clone())), Some(env));
        assert_eq!(decode(b"\0"), Some(vec![]));
    }

    #[test]
    fn report_requires_exactly_one_final_empty_record_and_nonempty_keys() {
        for bytes in [
            &b"A=a\0"[..],
            b"A=a",
            b"A=a\0\0trailing",
            b"A=a\0\0\0",
            b"A\0\0",
            b"=a\0\0",
            b"",
        ] {
            assert_eq!(decode(bytes), None, "{bytes:?}");
        }
    }
}
