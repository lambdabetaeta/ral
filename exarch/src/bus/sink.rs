//! The consuming end of the bus: the [`Sink`] presentation surface.

#[cfg(test)]
use super::BusReceiver;
use super::Signal;
use crate::record::{AgentId, Born, Record, Recorded, Transient};

/// A frontend fold over records and live signals.
pub trait Sink {
    /// Each record of a [`Signal::Commit`], in order, within one `accept`:
    /// witnessed, since a fold is stepped by the record *and* its locus.
    ///
    /// The default does nothing: a frontend draws whichever half of the seam
    /// it has a use for, and one that folds a session into blocks (headless)
    /// or narrates it as a stream (synod) overrides this.  A sink is handed
    /// the record only to step its own memo — never to render the record
    /// vocabulary from it, which it draws from
    /// [`BlockKind`](crate::record::BlockKind) off the memo instead, so a
    /// second hand-rolled projection cannot compile.  A sink narrates one
    /// exchange and keeps no cursor into the log.
    fn fact(&mut self, _id: AgentId, _rec: &Recorded<Record>) {}

    /// An agent's birth, ahead of anything it sends.
    fn born(&mut self, _id: AgentId, _born: &Born) {}

    /// A [`Transient`] delta reaching the sink live — no durable form and no
    /// sequence number, so only a frontend folding over `Transient` directly
    /// has a use for one.
    fn transient(&mut self, _id: AgentId, _t: &Transient) {}

    /// Route one raw signal to `fact`, `born` or `transient`.
    fn accept(&mut self, sig: Signal) {
        match sig {
            Signal::Commit(id, commit) => {
                for rec in commit.records() {
                    self.fact(id, rec);
                }
            }
            Signal::Born(id, born) => self.born(id, &born),
            Signal::Transient(id, t) => self.transient(id, &t),
        }
    }
}

/// Every [`Record`] a test's own channel has buffered, so a test asserts what
/// a producer actually recorded.
#[cfg(test)]
pub(crate) fn drain_records(rx: &BusReceiver) -> Vec<Record> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|sig| match sig {
            Signal::Commit(_, commit) => Some(commit.into_records()),
            Signal::Born(..) | Signal::Transient(..) => None,
        })
        .flatten()
        .map(Recorded::into_value)
        .collect()
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
