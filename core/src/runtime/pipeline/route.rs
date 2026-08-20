//! Interior edges of a process-staged pipeline are kernel byte pipes.
//!
//! Every edge is allocated before any stage spawns, and each end lives in
//! exactly one [`StageRoute`] the launcher consumes whole — so a doubly-wired
//! end is unrepresentable, and an aborted launch closes what it never spawned.
//!
//! Each non-final stage's route also carries a second, parent-held duplicate
//! of its outbound edge's read end ([`StageRoute::held`]).  While the parent
//! holds it, the stage writing to that edge can never observe its reader's
//! death — no SIGPIPE, no EPIPE — so the pipeline collector's kill is the only
//! channel through which a dead reader reaches its producer.  The duplicate
//! is dropped once the writer's own observation completes, releasing any of
//! the writer's descendants still blocked on the edge.

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

/// Whether the stage's `ChildEvalResponse` carries the pipeline's final
/// value.  Only the last stage of a value-yielding pipeline reports one — a
/// chatty decoder tail (`{ echo warn ; from-line }`) still has one to report.
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
    /// The parent's duplicate of this stage's outbound edge's read end;
    /// `None` for the final stage, which has no outbound edge.
    pub(super) held: Option<os_pipe::PipeReader>,
}

/// Allocate every interior edge as a byte pipe, purely from stage position —
/// every edge is the same kind of pipe regardless of what either side routes.
pub(super) fn open_stage_routes(plan: &PipelinePlan) -> Settled<Vec<StageRoute>> {
    let n = plan.specs.len();
    let mut routes = Vec::with_capacity(n);
    let mut inbound = None;
    for i in 0..n {
        let stdin = match inbound.take() {
            Some(reader) => ByteIn::Upstream(reader),
            None => ByteIn::Parent,
        };
        let (stdout, held) = if i + 1 < n {
            let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
            let held = r.try_clone().map_err(pipe_error)?;
            inbound = Some(r);
            (ByteOut::Downstream(w), Some(held))
        } else {
            (ByteOut::Parent, None)
        };
        let final_value = if i + 1 == n && plan.yields == crate::ir::PipeYield::Last {
            FinalValue::Report
        } else {
            FinalValue::Ignore
        };
        routes.push(StageRoute {
            stdin,
            stdout,
            final_value,
            held,
        });
    }
    Ok(routes)
}
