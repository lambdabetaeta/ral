//! What the fleet channel carries between a worker and a frontend: a
//! [`Signal`], in its two halves — a durable fact witnessed at append, and a
//! transient that never touches the log.  The inbound dual is `post`.

use super::AgentId;
use crate::record::{Record, Recorded, Transient};

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

/// What an agent is doing — a total state, not a label the next event erases.
///
/// Every moment of a session's life is one of these five, so a frontend can
/// always name the one it is in, including the idle one no label ever announced.
/// The transition is the delta ([`Transient::State`]); the states themselves
/// carry no clock and no counter.  A frontend times its own residence in one, which is
/// what makes a silent provider stream legible: [`Self::AwaitingModel`] standing
/// for minutes with no token arriving is a stall, where a label reset by each
/// arriving chunk could not tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Parked at a human-input boundary, or between one and the next: nothing
    /// is in flight and the prompt is the agent's own next move.
    Ready,
    /// A step is open — the request in flight, or its response streaming.  The
    /// two are one state because the boundary between them is not the worker's
    /// to know; a frontend tells them apart by whether anything has arrived.
    AwaitingModel,
    /// A `ral` call is evaluating.
    Evaluating,
    /// Summarising the history prefix to win back context.
    Compacting,
    /// Parked on a live child's result: a wait on the fleet, not on the human.
    WaitingOnAgents,
}

impl AgentState {
    /// The status-line label — lower case, unpunctuated; a frontend adds its
    /// own continuation mark to the [`Self::pending`] ones.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::AwaitingModel => "awaiting model",
            Self::Evaluating => "evaluating",
            Self::Compacting => "compacting",
            Self::WaitingOnAgents => "waiting on agents",
        }
    }

    /// Whether work is outstanding.  [`Self::Ready`] is the one settled state,
    /// so this is what a frontend keys a spinner, a repaint tick, or an
    /// ellipsis on.
    #[must_use]
    pub fn pending(self) -> bool {
        self != Self::Ready
    }
}
