//! Parent side of one ral helper stage: a re-execed
//! `--ral-pipeline-stage-helper` subprocess that blocks on a
//! [`ChildEvalRequest`] gate frame and answers with a [`ChildEvalResponse`].
//!
//! The last stage's value rides home inside that response, not on a value
//! pipe, so the report is read by its own thread while the helper still runs:
//! a large value or audit payload must not wedge the helper against the
//! parent's `wait`.

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
pub(super) struct HelperStageHandle {
    pub(super) running: command::RunningChild,
    pub(super) span: Option<crate::source::Span>,
    report: FrameReader<ChildEvalResponse>,
}

impl HelperStageHandle {
    /// Let the helper go without killing or reaping it, for a pipeline a
    /// sibling's stop has parked.  Dropping the `FrameReader` detaches its
    /// thread rather than stopping it; the thread sees EOF once a later `fg`
    /// runs the helper out.
    #[cfg(unix)]
    pub(super) fn abandon(self) {
        self.running.abandon();
    }

    /// Reduce one helper stage to a [`StageObservation`], the peer of
    /// `collect::observe_external_stage` for directly spawned externals.
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

        // The OS wait returned, so the helper's write end is closed and this
        // join cannot block.
        let report_result = report.join().map_err(pipe_error)?;

        // Report-first: a semantic error rides in the report and the helper
        // still exits 0, so the report outranks the OS outcome, which carries
        // no span or hint.
        if let Some(report) = report_result {
            let DecodedResponse {
                value,
                last_status,
                audit_nodes,
                signal,
                ..
            } = decode_response(report, shell)?;
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

        // No report: the helper died before writing one, so the OS outcome is
        // all there is.  A SIGPIPE forgiven on a non-final stage leaves no
        // failure and lands on the status-0 tail.
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
