//! Interior edges of a process-staged pipeline: a `Bytes` edge is one kernel
//! pipe, a `Value` edge one [`ValueChannel`].
//!
//! Every edge is allocated before any stage spawns, and each end lives in
//! exactly one [`StageRoute`] the launcher consumes whole — so a doubly-wired
//! end is unrepresentable, and an aborted launch closes what it never spawned.

use super::protocol::{ValueChannel, create_value_pair, pipe_error};
use super::resolve::PipelinePlan;
use crate::types::Settled;

/// Whether stage `i` receives a value rather than bytes: it has an upstream
/// and non-`Bytes` input.  This is data-last application (`x | f = f !{x}`),
/// so the consuming helper blocks on the edge before it invokes.
pub(super) fn value_edge_in(i: usize, comp_type: crate::mode::PipeSpec) -> bool {
    i > 0 && comp_type.input != crate::mode::PipeMode::Bytes
}

/// The dual of [`value_edge_in`]: stage `i` of `n` emits a value iff it has a
/// downstream and non-`Bytes` output.
pub(super) fn value_edge_out(i: usize, n: usize, comp_type: crate::mode::PipeSpec) -> bool {
    i + 1 < n && comp_type.output != crate::mode::PipeMode::Bytes
}

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
    /// The stage's output edge is a value channel, so its bytes go nowhere.
    Null,
}

/// Whether the stage's `ChildEvalResponse` carries the pipeline's final value.
/// Only the last value-typed ral stage reports one.
pub(super) enum FinalValue {
    Report,
    Ignore,
}

/// One stage's fully-wired endpoints, consumed by value at spawn.  At most one
/// value edge per side, the pipeline being linear; a directly spawned external
/// carries neither, since `resolve::direct_spawnable` refuses value edges.
pub(super) struct StageRoute {
    pub(super) stdin: ByteIn,
    pub(super) stdout: ByteOut,
    pub(super) value_in: Option<ValueChannel>,
    pub(super) value_out: Option<ValueChannel>,
    pub(super) final_value: FinalValue,
}

/// The consumer's half of an interior edge, carried across one loop iteration.
enum Inbound {
    Outer,
    Bytes(os_pipe::PipeReader),
    Value(ValueChannel),
}

/// Allocate every interior edge.  The producer's output mode alone picks each
/// edge's transport: the checker unified adjacent wires, so the consumer agrees.
pub(super) fn open_stage_routes(plan: &PipelinePlan) -> Settled<Vec<StageRoute>> {
    let n = plan.specs.len();
    let mut routes = Vec::with_capacity(n);
    let mut inbound = Inbound::Outer;
    for (i, spec) in plan.specs.iter().enumerate() {
        let (stdout, value_out, downstream) = if value_edge_out(i, n, spec.comp_type) {
            // The pair is (reader, writer), and on Windows the halves are
            // directional: the producer must keep the writer.
            let (r, w) = create_value_pair()?;
            (ByteOut::Null, Some(w), Inbound::Value(r))
        } else if i + 1 < n {
            let (r, w) = os_pipe::pipe().map_err(pipe_error)?;
            (ByteOut::Downstream(w), None, Inbound::Bytes(r))
        } else {
            (ByteOut::Parent, None, Inbound::Outer)
        };
        let (stdin, value_in) = match std::mem::replace(&mut inbound, downstream) {
            Inbound::Outer => (ByteIn::Parent, None),
            Inbound::Bytes(r) => (ByteIn::Upstream(r), None),
            Inbound::Value(ch) => (ByteIn::Parent, Some(ch)),
        };
        let final_value = if i + 1 == n && spec.comp_type.output != crate::mode::PipeMode::Bytes {
            FinalValue::Report
        } else {
            FinalValue::Ignore
        };
        routes.push(StageRoute {
            stdin,
            stdout,
            value_in,
            value_out,
            final_value,
        });
    }
    Ok(routes)
}
