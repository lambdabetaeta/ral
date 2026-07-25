//! Windows backend for the pipeline gate / report protocol.
//!
//! Channels are anonymous OS pipe pairs (`os_pipe::pipe()`) wrapped in a
//! Reader / Writer enum so the channel type is uniform with the Unix
//! socketpair backend.  A helper handle crosses only when `pass` writes its
//! numeric value into the helper environment and admits the raw handle to the
//! launch value.  The Windows launch backend supplies that allow-list to
//! `CreateProcessW` via `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` under the process
//! launch mutex.

use os_pipe::{PipeReader, PipeWriter};
use std::io::{Read, Write};

use super::super::helper::{
    JOB_HANDLE_ENV, REPORT_HANDLE_ENV, VALUE_IN_HANDLE_ENV, VALUE_OUT_HANDLE_ENV,
};
use super::common::{EnvNames, pipe_error};
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

/// Allocate one anonymous pipe.  Returns `(reader_end, writer_end)` so
/// the protocol can pass the read half into a child while the parent
/// keeps the write half (or vice versa, depending on direction).
pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
    Ok((Channel::Reader(r), Channel::Writer(w)))
}

/// Stash `ch`'s numeric handle value in `env` and admit that handle to this
/// one launch. The parent does not flip the inheritable bit here.
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

/// Env-var names this backend uses to pass channel handles to the helper.
pub(crate) const ENV: EnvNames = EnvNames {
    job: JOB_HANDLE_ENV,
    report: REPORT_HANDLE_ENV,
    value_in: VALUE_IN_HANDLE_ENV,
    value_out: VALUE_OUT_HANDLE_ENV,
};
