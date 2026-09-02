//! Platform-agnostic core of the pipeline gate / report protocol: the
//! parent's one-frame gate, the frame the launcher holds back, and the
//! background frame reader.  All of it is generic over `platform::Channel`,
//! so no descriptor API appears here — the `unix`, `windows`, and `fallback`
//! siblings supply `pair` and `pass`.

use std::io::BufReader;
use std::marker::PhantomData;
use std::thread;

use super::super::helper::{HELPER_FLAG, self_reexec};
use super::DeferredFrame;
use super::platform;
use crate::child_eval::{ChildEvalRequest, ChildEvalResponse};
use crate::types::{Break, Error, Settled};

pub(crate) fn pipe_error(e: impl std::fmt::Display) -> Break {
    Break::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Background reader for one length-prefixed frame on a child channel.
///
/// Drains concurrently with the child so a large frame cannot block its
/// writer while the parent waits on the OS process.  Drop does not stop the
/// thread: it exits at EOF, so a stopped or abandoned child needs no teardown.
pub(crate) struct FrameReader<T: serde::de::DeserializeOwned + Send + 'static> {
    handle: thread::JoinHandle<std::io::Result<Option<T>>>,
    panic_msg: &'static str,
}

impl<T: serde::de::DeserializeOwned + Send + 'static> FrameReader<T> {
    pub(super) fn spawn(stream: platform::Channel, panic_msg: &'static str) -> Self {
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            crate::subprocess_codec::read_frame::<_, T>(&mut reader)
        });
        Self { handle, panic_msg }
    }

    pub(crate) fn join(self) -> std::io::Result<Option<T>> {
        let msg = self.panic_msg;
        self.handle
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other(msg)))
    }
}

/// Parent side of a one-frame gate: the child blocks reading its inherited
/// end until `settle` and `PendingFrame::release` deliver the payload.
struct FrameGate<T> {
    writer: platform::Channel,
    child_end: Option<platform::Channel>,
    _phantom: PhantomData<fn(T)>,
}

impl<T: serde::Serialize + Send + 'static> FrameGate<T> {
    fn wire(cmd: &mut crate::process::Launch, env: &str) -> Settled<Self> {
        let (child_end, writer) = platform::pair()?;
        platform::pass(cmd, env, &child_end)?;
        Ok(Self {
            writer,
            child_end: Some(child_end),
            _phantom: PhantomData,
        })
    }

    /// Call only once spawn has returned: until then the child's end must
    /// stay open in the parent for the launch backend to hand it over.
    fn settle(mut self, payload: T) -> PendingFrame<T> {
        drop(self.child_end.take());
        PendingFrame {
            writer: self.writer,
            payload,
        }
    }
}

/// A gate frame the parent holds until every stage has spawned.  Dropping one
/// instead of releasing it closes the writer, so the child's blocked read sees
/// EOF: that is how an aborted launch tears its stages down.
pub(crate) struct PendingFrame<T> {
    writer: platform::Channel,
    payload: T,
}

impl<T: serde::Serialize + Send + 'static> PendingFrame<T> {
    /// Unblock the child.  `launch` calls this on every pending frame in one
    /// pass after `claim_foreground`, so no stage runs user code before the
    /// kernel's foreground decision is settled.
    pub(in super::super) fn release(self) -> Settled<()> {
        let Self {
            mut writer,
            payload,
        } = self;
        crate::subprocess_codec::write_frame(&mut writer, &payload).map_err(pipe_error)
    }
}

/// Parent-side ends of a helper stage's two channels — job gate in and report
/// out — held together because one backend table, `platform::ENV`, names both
/// for the helper to find them by.
pub(crate) struct HelperProtocol {
    job_gate: FrameGate<ChildEvalRequest>,
    child_report_writer: Option<platform::Channel>,
    report_reader: FrameReader<ChildEvalResponse>,
}

impl HelperProtocol {
    /// Re-exec ral behind `HELPER_FLAG` and wire the job/report channels onto
    /// it.
    pub(crate) fn build_command() -> Result<(crate::process::Launch, Self), Break> {
        let mut cmd = self_reexec(HELPER_FLAG).map_err(pipe_error)?;
        let proto = Self::wire(&mut cmd)?;
        Ok((cmd, proto))
    }

    fn wire(cmd: &mut crate::process::Launch) -> Settled<Self> {
        let env = platform::ENV;
        let job_gate = FrameGate::<ChildEvalRequest>::wire(cmd, env.job)?;
        let (report_reader_ch, report_writer) = platform::pair()?;
        platform::pass(cmd, env.report, &report_writer)?;
        let report_reader = FrameReader::spawn(report_reader_ch, "pipeline report reader panicked");

        Ok(Self {
            job_gate,
            child_report_writer: Some(report_writer),
            report_reader,
        })
    }

    /// After spawn: drop the parent's copies of the child's ends and queue the
    /// job frame.  The report writer especially must go — while the parent
    /// holds one, the report reader never sees EOF and its `join` never returns.
    pub(crate) fn settle(
        self,
        job: ChildEvalRequest,
    ) -> (FrameReader<ChildEvalResponse>, DeferredFrame) {
        let Self {
            job_gate,
            mut child_report_writer,
            report_reader,
        } = self;
        drop(child_report_writer.take());
        (report_reader, job_gate.settle(job))
    }
}

/// The env vars a backend uses to name its two channels to the helper, which
/// parses them back into descriptors in `helper::serve_from_env`.
pub(crate) struct EnvNames {
    pub(crate) job: &'static str,
    pub(crate) report: &'static str,
}
