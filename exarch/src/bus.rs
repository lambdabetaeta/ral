//! The agent / frontend boundary, in two halves: inbound, producers post
//! typed messages into a session's `Inbox` (`post`, `inbox`).
//!
//! Outbound, workers stamp facts through the record seam and those ride a
//! coalescing `channel` to a `Sink` as `Signal`s (`signal`, `channel`,
//! `emitter`, `sink`).

mod channel;
mod emitter;
mod inbox;
mod post;
mod signal;
mod sink;

pub use channel::{BusReceiver, BusSender, channel};
pub use emitter::Emitter;
pub use inbox::{Job, JobId};
pub use post::{AgentOutcome, ScheduleId};
pub use signal::Signal;
pub use sink::Sink;

pub(crate) use channel::MERGE_TEXT_CAP;
pub(crate) use emitter::{FleetBus, UsageMeter};
pub(crate) use inbox::{
    Child, Closed, Drawn, ENDED_BEFORE_REPORTING, Inbox, Lease, Mailbox, ParkMode, Queue, Refused,
    Report, Scoped, State, Status, WeakMailbox,
};
pub(crate) use post::{AgentMessage, Item, Next, Read, Rewrite};
pub(crate) use signal::WORKER_PANIC_PREFIX;
#[cfg(test)]
pub(crate) use sink::drain_records;
pub(crate) use sink::{Pass, Pumping, panic_msg, pump};

#[cfg(test)]
pub(crate) use emitter::{FleetSink, dummy_emitter};
