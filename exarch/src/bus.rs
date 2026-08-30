//! The agent / frontend boundary, in two halves: inbound, producers post
//! typed messages into a session's `Inbox` (`post`, `inbox`).
//!
//! Outbound, workers stamp facts through the record seam and those ride a
//! coalescing `channel` to a `Sink` as `Signal`s (`signal`, `channel`,
//! `emitter`, `sink`, `card`).

pub mod card;
mod channel;
mod emitter;
mod inbox;
mod post;
mod signal;
mod sink;

pub use channel::{BusReceiver, BusSender, channel};
pub use emitter::Emitter;
pub use inbox::Mailbox;
pub use post::AgentOutcome;
pub use signal::{AgentState, Signal};
pub use sink::Sink;

pub(crate) use channel::{MERGE_TEXT_CAP, WeakSender};
pub(crate) use emitter::{FleetBus, UsageMeter};
pub(crate) use inbox::{Inbox, ParkMode};
pub(crate) use post::{AgentMessage, AgentResult, Item, Post};
pub(crate) use signal::WORKER_PANIC_PREFIX;
#[cfg(test)]
pub(crate) use sink::drain_records;
pub(crate) use sink::{Pass, pump};

#[cfg(test)]
pub(crate) use emitter::dummy_emitter;

/// The identity of an agent node — the trunk and every forked child alike.
///
/// Opaque, and what crosses the wire: every `agents` tag and every `Signal`
/// names a node by this.  It is a routing key, not a door — an `agents` tag
/// resolves by name through the fleet, and the frontend matches an arriving
/// id against the tab it was handed at that agent's birth.
pub type AgentId = u64;
