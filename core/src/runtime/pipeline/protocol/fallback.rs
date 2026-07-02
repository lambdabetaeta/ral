//! Fallback backend for targets that are neither Unix nor Windows
//! (wasm and similar).  Process-staged pipelines have no transport on
//! such a target — there is no `self_reexec` to spawn a helper — so this
//! backend exists only to keep the protocol module compiling, mirroring
//! the `cfg(not(any(unix, windows)))` stubs in [`crate::process::signal`].
//! Every operation that would touch a channel errors; none is reachable.

use super::common::{EnvNames, FrameReader, pipe_error};
use crate::types::{Break, Settled};
use std::io::{Read, Write};

/// Stub channel: no transport exists, so both `Read` and `Write` error.
pub(crate) struct Channel;

impl Read for Channel {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(
            "pipeline channel unavailable on this target",
        ))
    }
}

impl Write for Channel {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(
            "pipeline channel unavailable on this target",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    Err(pipe_error(
        "process-staged pipelines are unavailable on this target",
    ))
}

pub(crate) fn pass(_cmd: &mut crate::process::Launch, _env: &str, _ch: &Channel) -> Settled<()> {
    Err(pipe_error(
        "process-staged pipelines are unavailable on this target",
    ))
}

pub(crate) fn reader<T>(ch: Channel, panic_msg: &'static str) -> FrameReader<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    FrameReader::spawn(ch, panic_msg)
}

pub(crate) const ENV: EnvNames = EnvNames {
    job: "RAL_PIPELINE_STAGE_JOB",
    report: "RAL_PIPELINE_STAGE_REPORT",
    value_in: "RAL_PIPELINE_STAGE_VALUE_IN",
    value_out: "RAL_PIPELINE_STAGE_VALUE_OUT",
};
