//! Platform-agnostic primitives for the pipeline gate / report protocol.
//!
//! [`FrameReader`] is a detached background thread that reads one
//! length-prefixed frame from a child-side channel.  [`FrameGate`] is
//! the parent side of a one-frame "release" gate — it owns the writer
//! end and the inheritable child-end channel until [`FrameGate::settle`]
//! turns it into a [`PendingFrame`] for the launcher to release.
//!
//! Everything in here is generic over the platform [`Channel`] type
//! re-exported by the parent module: on Unix the channel is a
//! socketpair, on Windows it is an anonymous-pipe Reader/Writer pair.  The
//! primitives don't reach into platform-specific APIs — that's all
//! tucked behind the [`platform::pass`] / [`platform::reader`]
//! backend functions in the parent module.

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

// ── FrameReader ────────────────────────────────────────────────────────────

/// Background reader for one length-prefixed frame on a child channel.
///
/// Used for the ral helper's [`ChildEvalResponse`]: drained concurrently
/// with the helper so a large report cannot block its writer while the
/// parent waits on the OS process.  The thread is detached on drop, so a
/// stopped or abandoned child does not need explicit teardown — the
/// reader exits naturally once its peer's write end closes.
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

// ── FrameGate ──────────────────────────────────────────────────────────────

/// Parent side of a one-frame gate.  Owns the writer end (parent
/// writes frames here) and the inheritable child-end channel (kept
/// alive until [`FrameGate::settle`] so spawn can inherit it, then
/// dropped so the child's read sees EOF on parent-side close — the
/// canonical abort path).  The phantom `T` ties the gate to one
/// payload type; [`FrameGate::settle`] turns it into a
/// [`PendingFrame<T>`] queued for release.
pub(crate) struct FrameGate<T> {
    writer: platform::Channel,
    child_end: Option<platform::Channel>,
    _phantom: PhantomData<fn(T)>,
}

impl<T: serde::Serialize + Send + 'static> FrameGate<T> {
    /// Allocate a channel pair, mark the child's reader end as
    /// inheritable, and stash its identity in `env` on `cmd`.  The
    /// parent keeps the writer end; the child end is held until
    /// [`FrameGate::settle`].
    pub(crate) fn wire(cmd: &mut crate::process::Launch, env: &str) -> Settled<Self> {
        let (child_end, writer) = platform::pair()?;
        platform::pass(cmd, env, &child_end)?;
        Ok(Self {
            writer,
            child_end: Some(child_end),
            _phantom: PhantomData,
        })
    }

    /// Drop the inheritable child-end channel and capture `payload`
    /// for a later release.  Call once the spawn has returned: until
    /// then the child's reader fd / handle must remain open in the
    /// parent so the platform launch backend can admit it into the child.
    pub(crate) fn settle(mut self, payload: T) -> PendingFrame<T> {
        let _ = self.child_end.take();
        PendingFrame {
            writer: self.writer,
            payload,
        }
    }
}

/// A queued frame waiting on the parent's release pass: writer is
/// connected to the child's read fd / handle, payload is the
/// serialized message that releases the gate.  The launcher collects
/// these in a `Vec` and writes them all once after `claim_foreground` /
/// `release_gate`, so every stage starts together.
pub(crate) struct PendingFrame<T> {
    writer: platform::Channel,
    payload: T,
}

impl<T: serde::Serialize + Send + 'static> PendingFrame<T> {
    /// Write the queued frame to the child's gate channel, unblocking the
    /// child.  Visible to the `pipeline` module so the launcher's
    /// release loop can call it directly on `PendingFrame<ChildEvalRequest>`.
    pub(in super::super) fn release(self) -> Settled<()> {
        let PendingFrame {
            mut writer,
            payload,
        } = self;
        crate::subprocess_codec::write_frame(&mut writer, &payload).map_err(pipe_error)
    }
}

// ── HelperProtocol ────────────────────────────────────────────────────────────

/// Parent-side state for a ral helper stage's protocol.
///
/// The gate frame ([`ChildEvalRequest`]) carries the work to evaluate, so
/// the protocol is always present (no `NoTerminal` skip).
///
/// Value-channel ends are passed through here only because the
/// inheritable channel dance and the env-var naming are part of the same
/// helper protocol; storing them on the gate keeps the launcher from
/// reaching into the backend env-var constants.
///
/// Generic over the platform [`platform::Channel`] re-exported by the
/// parent module; the four env-var names that pin the protocol to its
/// helper-side counterpart come from [`platform::ENV`].
pub(crate) struct HelperProtocol {
    job_gate: FrameGate<ChildEvalRequest>,
    child_report_writer: Option<platform::Channel>,
    report_reader: FrameReader<ChildEvalResponse>,
    incoming_value: Option<platform::Channel>,
    outgoing_value: Option<platform::Channel>,
}

impl HelperProtocol {
    /// Build the helper launch (re-exec ral with `HELPER_FLAG`)
    /// and wire job + report + optional value-channel envs.
    pub(crate) fn build_command(
        incoming_value: Option<platform::Channel>,
        outgoing_value: Option<platform::Channel>,
    ) -> Result<(crate::process::Launch, Self), Break> {
        let mut cmd = self_reexec(HELPER_FLAG).map_err(pipe_error)?;
        let proto = Self::wire(&mut cmd, incoming_value, outgoing_value)?;
        Ok((cmd, proto))
    }

    fn wire(
        cmd: &mut crate::process::Launch,
        incoming_value: Option<platform::Channel>,
        outgoing_value: Option<platform::Channel>,
    ) -> Settled<Self> {
        let env = platform::ENV;
        let job_gate = FrameGate::<ChildEvalRequest>::wire(cmd, env.job)?;
        let (report_reader_ch, report_writer) = platform::pair()?;
        platform::pass(cmd, env.report, &report_writer)?;
        let report_reader = platform::reader(report_reader_ch, "pipeline report reader panicked");

        // An absent value channel must be cleared, not merely left
        // unset: a helper-evaluated stage can itself launch a pipeline
        // (bundled-tool dispatch goes through `run_pipeline`), and the
        // spawning helper's own channel vars would otherwise ride the
        // inherited environment into the nested helper, which would
        // parse a fd number that is closed in its process.
        match incoming_value.as_ref() {
            Some(input) => platform::pass(cmd, env.value_in, input)?,
            None => {
                cmd.env_remove(env.value_in);
            }
        }
        match outgoing_value.as_ref() {
            Some(output) => platform::pass(cmd, env.value_out, output)?,
            None => {
                cmd.env_remove(env.value_out);
            }
        }

        Ok(Self {
            job_gate,
            child_report_writer: Some(report_writer),
            report_reader,
            incoming_value,
            outgoing_value,
        })
    }

    /// After spawn: drop the inheritable child-end channels (report
    /// writer, value channels), queue the [`ChildEvalRequest`] release.
    /// Returns the report reader and the deferred frame.
    pub(crate) fn settle(
        self,
        job: ChildEvalRequest,
    ) -> (FrameReader<ChildEvalResponse>, DeferredFrame) {
        let HelperProtocol {
            job_gate,
            mut child_report_writer,
            report_reader,
            incoming_value,
            outgoing_value,
        } = self;
        let _ = child_report_writer.take();
        drop(incoming_value);
        drop(outgoing_value);
        (report_reader, job_gate.settle(job))
    }
}

/// Backend-supplied env-var names for the four channels of the helper
/// protocol.  Unix uses `_FD` suffixes (the helper parses an integer fd
/// and re-opens via `from_raw_fd`); Windows uses `_HANDLE` suffixes
/// (the helper parses a numeric `HANDLE` and wraps via `from_raw_handle`).
pub(crate) struct EnvNames {
    pub(crate) job: &'static str,
    pub(crate) report: &'static str,
    pub(crate) value_in: &'static str,
    pub(crate) value_out: &'static str,
}
