//! Process-staged pipeline launcher for *ral helper* stages.
//!
//! A ral helper stage runs in a re-execed ral subprocess
//! (`--ral-pipeline-stage-helper`) that takes a [`ChildEvalRequest`]
//! frame from the parent and emits a [`ChildEvalResponse`] frame back.
//! The helper gates itself on the job frame — no user code runs before
//! the parent has finished spawning every stage and (interactive) called
//! `tcsetpgrp`.
//!
//! ## Final value transport
//!
//! The final ral helper stage's value comes back inside the
//! [`ChildEvalResponse`] frame, not on a separate value pipe.  A reader
//! thread per ral helper drains the report fd concurrently with the
//! helper running, so a large value/audit payload cannot block the
//! helper while the parent waits on the OS process.

use super::super::command;
use super::collect::StageObservation;
use super::group::PipelineGroup;
use super::launch::{spawn_into_group, wire_stage_stdio};
use super::protocol::{DeferredFrame, FrameReader, HelperProtocol, pipe_error};
use super::resolve::StageSpec;
use super::route::StageRoute;
use crate::child_eval::{ChildEvalRequest, ChildEvalResponse, DecodedResponse, decode_response};
use crate::types::{AuditFragment, Break, Error, Mooring, Settled, Shell};

/// A running ral helper stage: one process and one report-reader thread.
///
/// `report` drains the helper's structured outcome (status, audit nodes,
/// and — for the last value-typed stage — the final value).  The reader
/// is started concurrently with the helper so the helper cannot deadlock
/// on a full kernel buffer while the parent waits on the OS process.
///
/// On a stop, the reader thread is detached: it remains parked on the
/// stopped helper's pipe and exits naturally when the eventual `fg`
/// resumes the helper to completion.
pub(super) struct HelperStageHandle {
    pub(super) running: command::RunningChild,
    pub(super) span: Option<crate::source::Span>,
    report: FrameReader<ChildEvalResponse>,
}

impl HelperStageHandle {
    /// Abandon the running helper without killing or reaping it.  The
    /// report reader thread is detached implicitly: dropping the
    /// `FrameReader` does not stop the thread, and the thread exits on
    /// EOF when the helper eventually closes its write end.
    #[cfg(unix)]
    pub(super) fn abandon(self) {
        self.running.abandon();
    }

    /// Reduce one ral helper stage to a [`StageObservation`].
    /// Mirrors `collect::observe_external_stage` for direct externals:
    /// parked foreground jobs become control outcomes; helper semantic
    /// errors become failures; audit nodes ride along in the fragment
    /// for the collector to merge into the surrounding scope.
    pub(super) fn observe(
        self,
        shell: &Shell,
        is_last: bool,
        started: std::time::Instant,
    ) -> Settled<StageObservation> {
        let Self {
            running,
            span,
            report,
        } = self;
        let (helper_exit, failure) = match running.observe(!is_last) {
            Ok(pair) => pair,
            Err(br) => return Ok(StageObservation::from_break(br)),
        };

        // The OS wait has returned, so the helper's write end has
        // closed; the reader sees EOF or the last frame and the join
        // returns immediately.
        let report_result = report.join().map_err(pipe_error)?;

        // Report-first contract: a ral semantic error always rides in
        // the report and the helper exits 0.  We unpack before
        // consulting the OS outcome so the user sees the ral-level
        // diagnostic (with hint and span) rather than an
        // opaque process-failure shape.  Audit nodes — including
        // nested-external captures collected *before* the failure —
        // travel through the observation; the OS outcome only wins
        // when there is no report (helper killed, protocol-layer
        // failure).
        if let Some(report) = report_result {
            // Pipeline stages are subshells: the parent installs only
            // `last_status`, so the response's `mobile` and `surface_events`
            // are dropped (a stage child has no surface sink to replay to).
            let DecodedResponse {
                value,
                last_status,
                audit_nodes,
                signal,
                ..
            } = decode_response(report)?;
            let audit = AuditFragment::from_nodes(audit_nodes);
            if let Some(sig) = signal {
                return Ok(StageObservation::from_break(sig).with_audit(audit));
            }
            if helper_exit != 0 {
                let mut err = Error::new(
                    format!(
                        "pipeline helper exited with status {helper_exit} after reporting success"
                    ),
                    helper_exit,
                );
                err.span = span;
                return Ok(StageObservation::failure(err).with_audit(audit));
            }
            let final_value = if is_last { value } else { None };
            return Ok(StageObservation::ok(last_status)
                .with_value(final_value)
                .with_audit(audit));
        }

        // No report: the helper died before it could write one
        // (signal, kernel kill, sigkill on abort).  Fall back to the
        // OS wait outcome.  A forgiven failure (SIGPIPE on a non-final
        // stage) is success with status 0.
        if let Some(failure) = failure {
            let mut err = Error::from_command_failure("ral pipeline stage", failure, shell);
            err.span = span;
            let err = super::augment_stage_failure(err, shell, started);
            return Ok(StageObservation::failure(err));
        }
        Ok(StageObservation::ok(0))
    }
}

pub(super) fn launch_helper_stage(
    job: ChildEvalRequest,
    spec: &StageSpec,
    route: StageRoute,
    mooring: &Mooring,
    shell: &mut Shell,
    group: &mut PipelineGroup,
    park_on_stop: bool,
) -> Settled<(HelperStageHandle, DeferredFrame)> {
    let StageRoute {
        stdin,
        stdout,
        value_in,
        value_out,
        ..
    } = route;
    let (mut cmd, proto) = HelperProtocol::build_command(value_in, value_out)?;
    let plumbing = wire_stage_stdio(&mut cmd, stdin, stdout, group, shell)?;
    let running = spawn_into_group(
        group,
        &mut cmd,
        "ral pipeline stage".into(),
        plumbing,
        mooring,
        shell,
        park_on_stop,
        |e| {
            let mut err = Error::new(format!("pipeline helper: {e}"), 127);
            err.span = spec.span;
            Break::Error(err)
        },
    )?;

    let (report, deferred) = proto.settle(job);

    Ok((
        HelperStageHandle {
            running,
            span: spec.span,
            report,
        },
        deferred,
    ))
}
