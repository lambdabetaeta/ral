//! Interior edges of a process-staged pipeline are kernel byte pipes.
//!
//! Every edge is allocated before any stage spawns, and each end lives in
//! exactly one [`StageRoute`] the launcher consumes whole — so a doubly-wired
//! end is unrepresentable, and an aborted launch closes what it never spawned.

use super::protocol::pipe_error;
use super::resolve::PipelinePlan;
use crate::types::Settled;

/// Stdin source for one stage.
pub(super) enum ByteIn {
    /// The pipeline's input boundary; `launch::route_stdin` resolves it
    /// against the enclosing shell's stdin.
    Parent,
    Upstream(os_pipe::PipeReader),
}

/// Stdout destination for one stage.
pub(super) enum ByteOut {
    /// The pipeline's output boundary; `launch::wire_stage_stdout` resolves it
    /// against the shell's stdout sink.
    Parent,
    Downstream(os_pipe::PipeWriter),
}

/// Whether the stage's `ChildEvalResponse` carries the pipeline's final value.
/// Only the last stage whose *payload* is a value reports one — a chatty
/// decoder tail (`{ echo warn ; from-line }`) still has a value to report.
pub(super) enum FinalValue {
    Report,
    Ignore,
}

/// One stage's fully-wired byte endpoints, consumed by value at spawn. A
/// directly spawned external carries the same route shape as a helper.
pub(super) struct StageRoute {
    pub(super) stdin: ByteIn,
    pub(super) stdout: ByteOut,
    pub(super) final_value: FinalValue,
}

/// Allocate every interior edge as a byte pipe.  The checker unified adjacent
/// wires, so the consumer agrees with the producer's byte payload.
pub(super) fn open_stage_routes(plan: &PipelinePlan) -> Settled<Vec<StageRoute>> {
    let n = plan.specs.len();
    let mut routes = Vec::with_capacity(n);
    let mut inbound = None;
    for (i, spec) in plan.specs.iter().enumerate() {
        let stdin = match inbound.take() {
            Some(reader) => ByteIn::Upstream(reader),
            None => ByteIn::Parent,
        };
        let stdout = if i + 1 < n {
            let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
            inbound = Some(r);
            ByteOut::Downstream(w)
        } else {
            ByteOut::Parent
        };
        let final_value = if i + 1 == n && spec.comp_type.result != crate::mode::PipeMode::Bytes {
            FinalValue::Report
        } else {
            FinalValue::Ignore
        };
        routes.push(StageRoute {
            stdin,
            stdout,
            final_value,
        });
    }
    Ok(routes)
}
