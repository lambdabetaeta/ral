//! The agent / frontend boundary: producers post typed messages into a
//! session's [`Inbox`]; workers stamp [`Kind`]s with their [`AgentId`]
//! through an [`Emitter`], consumed by a [`Sink`].
//!
//! The boundary has two halves: inbound is [`post`] and [`inbox`];
//! outbound is [`event`] and [`channel`], with [`emitter`] the producing
//! end and [`sink`] the consuming end.
//!
//! - [`post`] — what may arrive: a message as posted (`Post`), and the
//!   [`Item`] it becomes once drained into context.
//! - [`inbox`] — the queue itself: [`Mailbox`] senders and the [`Inbox`]
//!   consumer, the coalesce-or-quota push rule, and the two drains; the
//!   inbox-before-registry lock order lives there.
//! - [`event`] — the closed vocabulary of everything a worker can tell a
//!   frontend.
//! - [`channel`] — the bounded, coalescing transport those events ride.
//! - [`emitter`] — the handle a worker stamps events through, and the
//!   bus that mints them.
//! - [`sink`] — one presentation surface, and the completion contract
//!   that ends an exchange.
//! - [`card`] — the render document a surface carries.

pub mod card;
mod channel;
mod emitter;
mod event;
mod inbox;
mod post;
mod sink;

pub use channel::{BusReceiver, BusSender, channel};
pub use emitter::Emitter;
pub use event::{Event, Kind};
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

/// The identity of an agent node.
///
/// Every agent — the trunk and every forked
/// child alike — has one; a child's id *is* its `AgentId`, so the `agents`
/// listing and `agent-cancel` reuse the node identity rather than minting a
/// parallel one.  Opaque: a capability for status and cancellation, not a
/// content hash.
pub type AgentId = u64;
