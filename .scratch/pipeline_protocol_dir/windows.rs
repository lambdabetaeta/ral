//! Windows backend for the pipeline gate / report protocol.
//!
//! Channels are anonymous OS pipes; the Reader / Writer enum collapses their
//! two halves into the one `Channel` type `common` is generic over.  `pass`
//! only names a handle in the child's environment and admits it to the
//! `Launch`; `Launch::spawn` sets `HANDLE_FLAG_INHERIT` and hands the admitted
//! handles to `CreateProcessW` as a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, both
//! under a process-wide mutex.

use os_pipe::{PipeReader, PipeWriter};
use std::io::{Read, Write};

use super::super::helper::{JOB_HANDLE_ENV, REPORT_HANDLE_ENV};
use super::common::{EnvNames, pipe_error};
use crate::types::{Break, Settled};

/// One half of an anonymous OS pipe.  A mismatched read or write errors
/// rather than being unrepresentable; call sites uphold the direction.
pub(crate) enum Channel {
    Reader(PipeReader),
    Writer(PipeWriter),
}

impl Channel {
    fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        match self {
            Self::Reader(r) => r.as_raw_handle(),
            Self::Writer(w) => w.as_raw_handle(),
        }
    }
}

impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Reader(r) => r.read(buf),
            Self::Writer(_) => Err(std::io::Error::other(
                "pipeline channel: read from writer end",
            )),
        }
    }
}

impl Write for Channel {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Writer(w) => w.write(buf),
            Self::Reader(_) => Err(std::io::Error::other(
                "pipeline channel: write to reader end",
            )),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Writer(w) => w.flush(),
            Self::Reader(_) => Ok(()),
        }
    }
}

/// Allocate one anonymous pipe as `(reader, writer)`.  Callers pick an end by
/// position, so unlike the symmetric Unix socketpair the order binds.
pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
    Ok((Channel::Reader(r), Channel::Writer(w)))
}

/// Name `ch`'s handle in `env` and admit it to this one launch.  The handle
/// travels raw, so `ch` must stay alive until the spawn returns.
#[allow(
    clippy::unnecessary_wraps,
    reason = "one of the platform `pass` backends behind `platform::pass`; the fallback variant genuinely returns `Err`, so the `Settled<()>` signature is fixed across the backend family."
)]
pub(crate) fn pass(cmd: &mut crate::process::Launch, env: &str, ch: &Channel) -> Settled<()> {
    let handle = ch.raw_handle();
    cmd.env(env, (handle as usize).to_string());
    cmd.admit_handle(handle);
    Ok(())
}

/// Env-var names carrying channel handles to the helper.
pub(crate) const ENV: EnvNames = EnvNames {
    job: JOB_HANDLE_ENV,
    report: REPORT_HANDLE_ENV,
};
