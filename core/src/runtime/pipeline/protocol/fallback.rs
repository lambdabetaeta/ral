//! Stub backend for targets that are neither Unix nor Windows.  Process-staged
//! pipelines need `self_reexec`, which only the Unix and Windows arms of
//! `helper` define, so nothing here is reachable: it keeps `protocol` compiling.

use super::common::{EnvNames, pipe_error};
use crate::types::{Break, Settled};
use std::io::{Read, Write};

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

pub(crate) const ENV: EnvNames = EnvNames {
    job: "RAL_PIPELINE_STAGE_JOB",
    report: "RAL_PIPELINE_STAGE_REPORT",
    value_in: "RAL_PIPELINE_STAGE_VALUE_IN",
    value_out: "RAL_PIPELINE_STAGE_VALUE_OUT",
};
