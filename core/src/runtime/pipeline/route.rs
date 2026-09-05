//! Interior edges are kernel byte pipes, each end living in exactly one
//! [`StageRoute`].  The parent's own duplicate of a non-final read end
//! ([`HeldEdge`]) keeps EPIPE off every writer: the sentinel hears it instead.

use crate::io::Edge;
use crate::types::{Break, Error, Settled};
use std::sync::Arc;

pub(super) fn pipe_error(e: impl std::fmt::Display) -> Break {
    Break::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Stdin source for one stage.
pub(super) enum ByteIn {
    /// The pipeline's input boundary, resolved against the enclosing shell's
    /// stdin.
    Parent,
    Upstream(os_pipe::PipeReader),
}

/// Stdout destination for one stage.
pub(super) enum ByteOut {
    /// The pipeline's output boundary, resolved against the shell's stdout
    /// sink.
    Parent,
    Downstream(os_pipe::PipeWriter, Arc<Edge>),
}

/// The parent's hold on a non-final stage's outbound edge: its fate, and a
/// duplicate of its read end, shared with the sentinel that reads it.
pub(super) struct HeldEdge {
    pub(super) edge: Arc<Edge>,
    pub(super) reader: Arc<os_pipe::PipeReader>,
}

/// One stage's fully-wired byte endpoints, consumed by value at spawn.
pub(super) struct StageRoute {
    pub(super) stdin: ByteIn,
    pub(super) stdout: ByteOut,
    /// `None` for the final stage, which has no outbound edge.
    pub(super) held: Option<HeldEdge>,
}

fn open_edge() -> Settled<((ByteOut, Option<HeldEdge>), ByteIn)> {
    let (r, w) = crate::process::cloexec_pipe().map_err(pipe_error)?;
    let edge = Edge::new();
    Ok((
        (
            ByteOut::Downstream(w, Arc::clone(&edge)),
            Some(HeldEdge {
                edge,
                reader: Arc::new(r.try_clone().map_err(pipe_error)?),
            }),
        ),
        ByteIn::Upstream(r),
    ))
}

/// Every edge is the same pipe, so position alone decides.
pub(super) fn open_stage_routes(n: usize) -> Settled<Vec<StageRoute>> {
    let (outs, ins): (Vec<_>, Vec<_>) = (1..n)
        .map(|_| open_edge())
        .collect::<Settled<Vec<_>>>()?
        .into_iter()
        .unzip();
    let stdins = std::iter::once(ByteIn::Parent).chain(ins);
    let stdouts = outs
        .into_iter()
        .chain(std::iter::once((ByteOut::Parent, None)));
    Ok(stdins
        .zip(stdouts)
        .map(|(stdin, (stdout, held))| StageRoute {
            stdin,
            stdout,
            held,
        })
        .collect())
}
