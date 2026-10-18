//! What the fleet channel carries between a worker and a frontend: a
//! [`Signal`] — a durable fact witnessed at append, an agent's birth, or a
//! transient that never touches the log.  The inbound dual is `post`.

use crate::record::{AgentId, Born, Record, Recorded, Segment, Transient};

/// What the fleet channel carries, tagged with the [`AgentId`] it concerns.
///
/// One channel, so the TUI's single per-fleet dispatch loop routes all of it
/// by [`AgentId`] without restructuring.
pub enum Signal {
    /// A record, stamped with the segment and sequence number its append gave
    /// it.
    Fact(AgentId, Segment, Recorded<Record>),
    /// An agent's birth: sent by minting its emitter, so ahead of anything
    /// that emitter carries.
    Born(AgentId, Born),
    /// A live-only delta — a token, a state, the seam's own fault — with no
    /// durable form and no sequence number of its own.
    Transient(AgentId, Transient),
}

/// Prefix of the [`crate::record::Forensic::Error`] a recovered worker panic
/// records, so a sink can tell one from a clean completion without matching
/// free text.
pub(crate) const WORKER_PANIC_PREFIX: &str = "worker panicked: ";
