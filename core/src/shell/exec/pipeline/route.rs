//! Interior edges are kernel byte pipes, each end living in exactly one
//! [`StageRoute`].  The parent's own duplicate of a non-final read end
//! ([`HeldEdge`]) keeps EPIPE off every writer: the sentinel hears it instead.

use crate::io::Edge;
use crate::shell::runtime::{Error, Settled};
use std::cell::Cell;
use std::sync::Arc;

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
    /// An interior edge's write end, and the parent's hold on it.
    Downstream {
        writer: os_pipe::PipeWriter,
        held: HeldEdge,
    },
}

/// The parent's hold on a non-final stage's outbound edge: its fate, and a
/// duplicate of its read end, which the hold alone keeps open.
pub(super) struct HeldEdge {
    pub(super) edge: Arc<Edge>,
    pub(super) reader: Arc<os_pipe::PipeReader>,
    /// Set once a sentinel reads it: one per edge.
    pub(super) listening: Cell<bool>,
}

/// One stage's fully-wired byte endpoints, consumed by value at spawn.
pub(super) struct StageRoute {
    pub(super) stdin: ByteIn,
    pub(super) stdout: ByteOut,
}

fn open_edge() -> Settled<(ByteOut, ByteIn)> {
    let (r, w) = crate::process::cloexec_pipe().map_err(|e| Error::io("pipe", &e))?;
    let held = HeldEdge {
        edge: Edge::new(),
        reader: Arc::new(r.try_clone().map_err(|e| Error::io("pipe", &e))?),
        listening: Cell::default(),
    };
    Ok((ByteOut::Downstream { writer: w, held }, ByteIn::Upstream(r)))
}

/// Every edge is the same pipe, so position alone decides.
pub(super) fn open_stage_routes(n: usize) -> Settled<Vec<StageRoute>> {
    let (outs, ins): (Vec<_>, Vec<_>) = (1..n)
        .map(|_| open_edge())
        .collect::<Settled<Vec<_>>>()?
        .into_iter()
        .unzip();
    let stdins = std::iter::once(ByteIn::Parent).chain(ins);
    let stdouts = outs.into_iter().chain(std::iter::once(ByteOut::Parent));
    Ok(stdins
        .zip(stdouts)
        .map(|(stdin, stdout)| StageRoute { stdin, stdout })
        .collect())
}
