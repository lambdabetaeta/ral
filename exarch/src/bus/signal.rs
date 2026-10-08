//! What the fleet channel carries between a worker and a frontend: a
//! [`Signal`], in its two halves — a durable fact witnessed at append, and a
//! transient that never touches the log.  The inbound dual is `post`.

use crate::record::{AgentId, Record, Recorded, Transient};

/// The two passengers the record seam publishes, tagged with the [`AgentId`]
/// that produced them.
///
/// One channel, so the TUI's single per-fleet dispatch loop routes both by
/// [`AgentId`] without restructuring.
pub enum Signal {
    /// A record, stamped with the sequence number its append gave it.
    Fact(AgentId, Recorded<Record>),
    /// A live-only delta — a token, a state, the seam's own fault — with no
    /// durable form and no sequence number of its own.
    Transient(AgentId, Transient),
}

/// Prefix of the [`crate::record::Forensic::Error`] a recovered worker panic
/// records, so a sink can tell one from a clean completion without matching
/// free text.
pub(crate) const WORKER_PANIC_PREFIX: &str = "worker panicked: ";
