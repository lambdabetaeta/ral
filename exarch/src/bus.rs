//! The agent / frontend boundary, in two halves: inbound, producers post
//! typed messages into a session's `Inbox` (`post`, `inbox`); outbound,
//! workers stamp events through an `Emitter` and those ride a coalescing
//! `channel` to a `Sink` (`event`, `channel`, `emitter`, `sink`, `card`).

pub mod card;
mod channel;
mod emitter;
mod event;
mod inbox;
mod post;
mod sink;

pub use channel::{BusReceiver, BusSender, channel};
pub use emitter::Emitter;
pub use event::{AgentState, Event, Kind};
pub use inbox::{InboxReject, Mailbox};
pub use post::AgentOutcome;
pub use sink::Sink;

pub(crate) use channel::MERGE_TEXT_CAP;
pub(crate) use emitter::FleetBus;
pub(crate) use event::WORKER_PANIC_PREFIX;
pub(crate) use inbox::{INBOX_SOURCE_CAP, INBOX_TOTAL_CAP, Inbox, ParkMode};
pub(crate) use post::{AgentMessage, AgentResult, Item, Post};
pub(crate) use sink::{Pass, drain_pass, pump};

#[cfg(test)]
pub(crate) use emitter::dummy_emitter;

/// The identity of an agent node — the trunk and every forked child alike.
/// Opaque, and the fleet registry's key: `agents` and `agent-cancel` name a
/// node by this and nothing else.
pub type AgentId = u64;
