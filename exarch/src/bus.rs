//! The agent / frontend boundary, in two halves: inbound, producers post
//! typed messages to an agent's mailbox (`post`, `mailbox`).
//!
//! Outbound, workers stamp facts through the record seam and those ride a
//! coalescing `channel` to a `Sink` as `Signal`s (`signal`, `channel`,
//! `emitter`, `sink`).

mod channel;
mod emitter;
mod lease;
mod mailbox;
mod post;
mod signal;
mod sink;

pub use channel::BusReceiver;
#[cfg(any(test, feature = "test-util"))]
pub use channel::{BusSender, channel};
pub use emitter::Emitter;
pub use mailbox::{Admission, Command};
pub use post::{AgentOutcome, ScheduleId};
pub use post::{Read, Rewrite};
pub use signal::Signal;
pub use sink::Sink;

pub(crate) use channel::MERGE_TEXT_CAP;
pub(crate) use emitter::{FleetBus, UsageMeter};
pub(crate) use lease::Lease;
pub(crate) use mailbox::{
    AgentAddress, AgentMailbox, Drained, Input, Next, Refused, Rest, Status, WeakAgentAddress,
};
pub(crate) use post::{AgentMessage, ENDED_BEFORE_REPORTING, notice, surface_notice};
pub(crate) use signal::WORKER_PANIC_PREFIX;
#[cfg(test)]
pub(crate) use sink::drain_records;
pub(crate) use sink::panic_msg;

#[cfg(test)]
pub(crate) use emitter::{FleetSink, dummy_emitter};
