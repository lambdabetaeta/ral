//! Per-stage routing for process-staged pipelines.
//!
//! A pipeline is *n* stages separated by *n−1* typed edges.  Every
//! edge is allocated here, once, before any stage spawns: a `Bytes`
//! edge is one kernel pipe, a `Value` edge one serialised-value
//! channel ([`ValueChannel`]).  Each end is moved into the
//! [`StageRoute`] of the stage that uses it, and a route is consumed
//! whole by the launcher — ownership makes a leaked or doubly-wired
//! edge end unrepresentable, and an aborted launch closes every
//! unconsumed end by dropping the remaining routes.

use super::protocol::{ValueChannel, create_value_pair, pipe_error};
use super::resolve::PipelinePlan;
use crate::types::Settled;

/// Whether stage `i` receives a value on its input edge.
///
/// A value edge is data-last application (`x | f = f !{x}`): the
/// upstream value must arrive before the stage invokes.  Stage `i`
/// receives one iff it has an upstream (`i > 0`) and its input mode is
/// non-`Bytes`.  [`open_stage_routes`] realizes this edge as the
/// consumer's [`ValueChannel`] half.
pub(super) fn value_edge_in(i: usize, comp_type: crate::mode::PipeSpec) -> bool {
    i > 0 && comp_type.input != crate::mode::PipeMode::Bytes
}

/// Whether stage `i` of `n` emits a value on its output edge.
///
/// The dual of [`value_edge_in`]: stage `i` emits one iff it has a
/// downstream (`i + 1 < n`) and its output mode is non-`Bytes`.
/// [`open_stage_routes`] realizes this edge as the producer's
/// [`ValueChannel`] half.
pub(super) fn value_edge_out(i: usize, n: usize, comp_type: crate::mode::PipeSpec) -> bool {
    i + 1 < n && comp_type.output != crate::mode::PipeMode::Bytes
}

/// Stdin source for one stage.
pub(super) enum ByteIn {
    /// Pipeline boundary on the input side: the launcher routes this
    /// stage's stdin against the shell's stdin (Inherit on a tty when
    /// the pipeline owns it, Null otherwise).
    Parent,
    /// Reader end of the byte edge from the upstream stage.
    Upstream(os_pipe::PipeReader),
}

/// Stdout destination for one stage.
pub(super) enum ByteOut {
    /// Pipeline boundary on the output side: the launcher routes this
    /// stage's stdout against the shell's stdout (Inherit on a tty +
    /// no audit, Pump-with-optional-tee otherwise).
    Parent,
    /// Writer end of the byte edge to the downstream stage.
    Downstream(os_pipe::PipeWriter),
    /// Stage emits values, not bytes (its interior output edge is a
    /// value channel); discard byte output via `Stdio::null()`.
    Null,
}

/// Whether the stage's `ChildEvalResponse` carries the pipeline's final
/// value back to the parent.  `Report` only applies to the last
/// value-typed ral stage; everything else (byte-mode last stages,
/// every non-final stage) is `Ignore`.
pub(super) enum FinalValue {
    Report,
    Ignore,
}

/// One stage's fully-wired endpoints, consumed by value at spawn.
///
/// A stage has at most one incoming and at most one outgoing value
/// edge because the pipeline is linear; externals never carry either
/// end.  Both Unix and Windows back the value channel with a real
/// pipe pair; on Windows the channel is a Reader/Writer enum over an
/// anonymous OS pipe rather than a Unix-domain socketpair half.
pub(super) struct StageRoute {
    pub(super) stdin: ByteIn,
    pub(super) stdout: ByteOut,
    pub(super) value_in: Option<ValueChannel>,
    pub(super) value_out: Option<ValueChannel>,
    pub(super) final_value: FinalValue,
}

/// The consumer's half of an interior edge, handed from a producer
/// stage to its successor while the routes are built.
enum Inbound {
    Outer,
    Bytes(os_pipe::PipeReader),
    Value(ValueChannel),
}

/// Allocate every interior edge and distribute the ends into one
/// [`StageRoute`] per stage.  The producer's output mode decides each
/// edge's transport — a byte pipe when it emits bytes, a value channel
/// otherwise; adjacency (the checker unified the wires it emitted)
/// guarantees the consumer agrees.  On any error the partly-built
/// `Vec<StageRoute>` drops and every allocated end closes.
pub(super) fn open_stage_routes(plan: &PipelinePlan) -> Settled<Vec<StageRoute>> {
    let n = plan.specs.len();
    let mut routes = Vec::with_capacity(n);
    let mut inbound = Inbound::Outer;
    for (i, spec) in plan.specs.iter().enumerate() {
        // The last stage's output is the pipeline boundary; an interior
        // edge is a value channel when the producer emits a value edge,
        // otherwise a byte pipe.
        let (stdout, value_out, downstream) = if value_edge_out(i, n, spec.comp_type) {
            // `create_value_pair` returns (reader_end, writer_end) on both
            // backends.  The producer keeps the writer; the consumer gets
            // the reader.
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
