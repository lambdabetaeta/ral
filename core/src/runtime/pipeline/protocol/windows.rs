//! Windows backend for the pipeline gate / report protocol.
//!
//! Channels are anonymous OS pipe pairs (`os_pipe::pipe()`) wrapped in
//! a Reader / Writer enum so the channel type is uniform with the Unix
//! socketpair backend.  Inheritance is `SetHandleInformation(handle,
//! HANDLE_FLAG_INHERIT)` plus a numeric handle value stashed in the
//! helper's env; the helper wraps that value via `from_raw_handle` and
//! clears `HANDLE_FLAG_INHERIT` (the Windows analogue of
//! `FD_CLOEXEC`) so the handle does not leak into nested children.
//!
//! `std::process::Command` on Windows sets `bInheritHandles=TRUE` only
//! when at least one of stdin/stdout/stderr is `Stdio::piped()`.  Every
//! helper this protocol drives has piped stdio installed by the
//! launcher (byte channel wiring), so the inherit bit is always set;
//! no custom `CreateProcessW` wrapper is required.

use std::io::{Read, Write};
use std::process::Command;

use os_pipe::{PipeReader, PipeWriter};

use super::super::helper::{
    JOB_HANDLE_ENV, REPORT_HANDLE_ENV, VALUE_IN_HANDLE_ENV, VALUE_OUT_HANDLE_ENV,
};
use super::common::{EnvNames, FrameReader, pipe_error};
use crate::types::{Break, Settled};

/// Windows-side channel: one half of an anonymous OS pipe.
///
/// `Channel::pair()` returns `(Reader, Writer)`.  Both ends own their
/// underlying handle so dropping closes them deterministically.  The
/// `Read` / `Write` impls only succeed on the matching variant; calling
/// `Write::write` on a `Reader` (or vice versa) returns an I/O error, and
/// `flush` on a `Reader` is a no-op.  The protocol's call sites uphold the
/// variant invariant anyway (gate writers only ever pass a writer; gate
/// readers only ever pass a reader).
pub(crate) enum Channel {
    Reader(PipeReader),
    Writer(PipeWriter),
}

impl Channel {
    fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        match self {
            Channel::Reader(r) => r.as_raw_handle(),
            Channel::Writer(w) => w.as_raw_handle(),
        }
    }
}

impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Channel::Reader(r) => r.read(buf),
            Channel::Writer(_) => Err(std::io::Error::other(
                "pipeline channel: read from writer end",
            )),
        }
    }
}

impl Write for Channel {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Channel::Writer(w) => w.write(buf),
            Channel::Reader(_) => Err(std::io::Error::other(
                "pipeline channel: write to reader end",
            )),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Channel::Writer(w) => w.flush(),
            Channel::Reader(_) => Ok(()),
        }
    }
}

/// Allocate one anonymous pipe.  Returns `(reader_end, writer_end)` so
/// the protocol can pass the read half into a child while the parent
/// keeps the write half (or vice versa, depending on direction).
pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
    Ok((Channel::Reader(r), Channel::Writer(w)))
}

/// Mark `ch`'s underlying handle inheritable and stash its numeric
/// value (as decimal text) in `env` on `cmd`.  The helper parses the
/// integer, wraps the handle via `from_raw_handle`, and clears the
/// inherit bit so it does not leak further.
pub(crate) fn pass(cmd: &mut Command, env: &str, ch: &Channel) -> Settled<()> {
    let handle = ch.raw_handle();
    set_inheritable(handle)?;
    cmd.env(env, format!("{}", handle as usize));
    Ok(())
}

/// Spawn a [`FrameReader`] reading from the given channel.  Identical
/// shape to the Unix backend; the read path is whichever variant the
/// channel carries (gate report channels are always readers).
pub(crate) fn reader<T>(ch: Channel, panic_msg: &'static str) -> FrameReader<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    FrameReader::spawn(ch, panic_msg)
}

fn set_inheritable(handle: std::os::windows::io::RawHandle) -> Settled<()> {
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::{HANDLE, SetHandleInformation};
    let ok =
        unsafe { SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    if ok == 0 {
        return Err(pipe_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Env-var names this backend uses to pass channel handles to the helper.
pub(crate) const ENV: EnvNames = EnvNames {
    job: JOB_HANDLE_ENV,
    report: REPORT_HANDLE_ENV,
    value_in: VALUE_IN_HANDLE_ENV,
    value_out: VALUE_OUT_HANDLE_ENV,
};
