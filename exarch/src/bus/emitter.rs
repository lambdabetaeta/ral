//! The producing end of the bus: [`Emitter`], the handle a worker stamps its
//! events through, and [`FleetBus`], which owns the channel and mints emitters.

use super::AgentId;
use super::channel::{BusReceiver, BusSender, channel};
use super::event::{Event, Kind};
use super::inbox::{Inbox, Mailbox};
use crate::agent::transcript::Transcript;
use crate::provider::Usage;
use crate::sync::LockExt;
use std::sync::{Arc, Mutex};

/// A usage accumulator shared by every emitter of one run — where a
/// [`Transcript`] is per-session, this is per-run.  Usage tees here at the emit
/// seam, so a display-muted child whose events reach no sink still counts
/// toward the total.
#[derive(Clone, Default)]
pub(crate) struct UsageMeter(Arc<Mutex<Usage>>);

impl UsageMeter {
    pub(crate) fn add(&self, u: Usage) {
        *self.0.lock_ignore_poison() += u;
    }

    pub(crate) fn total(&self) -> Usage {
        *self.0.lock_ignore_poison()
    }
}

#[derive(Clone)]
pub struct Emitter {
    tx: BusSender,
    id: AgentId,
    /// The owning session's mailbox — root's for the root emitter, the child's
    /// own for a child.  Never another agent's.
    mailbox: Mailbox,
    /// Whether the channel outlives the spawning exchange, so a detached async
    /// `agent` child may clone this emitter for a live tab.  Display only: a
    /// muted child still records and still meters.
    session_lived: bool,
    transcript: Transcript,
    meter: UsageMeter,
}

impl Emitter {
    /// An emitter with an orphan mailbox and no transcript — for tests, whose
    /// events land nowhere durable.
    pub fn new(tx: BusSender, id: AgentId) -> Self {
        Self::with_mailbox(tx, id, Inbox::new().mailbox())
    }

    /// [`Self::new`] over a caller-supplied mailbox; still transcript-less.
    pub(crate) fn with_mailbox(tx: BusSender, id: AgentId, mailbox: Mailbox) -> Self {
        Self {
            tx,
            id,
            mailbox,
            session_lived: false,
            transcript: Transcript::none(),
            meter: UsageMeter::default(),
        }
    }

    /// A child emitter onto a channel whose receiver is already dropped, so it
    /// streams nowhere, but carrying a live [`Transcript`] and the parent run's
    /// meter.  What an async `agent` child takes off a per-exchange bus.
    pub(crate) fn muted_child(&self, id: AgentId, transcript: Transcript) -> Self {
        let (tx, _rx) = channel();
        Self {
            tx,
            id,
            mailbox: Inbox::new().mailbox(),
            session_lived: false,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// A sibling emitter on the same channel for a child session: the child's
    /// own mailbox and trace, never the parent's, but the shared run meter.
    pub(crate) fn child(&self, id: AgentId, mailbox: Mailbox, transcript: Transcript) -> Self {
        Self {
            tx: self.tx.clone(),
            id,
            mailbox,
            session_lived: self.session_lived,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// Record, meter, *then* send — in that order, so neither the trace nor the
    /// run total depends on a live receiver ([`Self::muted_child`]'s point).
    pub(crate) fn emit(&self, kind: Kind) {
        self.transcript.record(self.id, &kind);
        if let Kind::Usage(u) = &kind {
            self.meter.add(*u);
        }
        let _ = self.tx.send(Event { id: self.id, kind });
    }

    /// The owning session's mailbox, for `shell_eval::deferred_sink`, which
    /// posts a spawn worker's surface batch back into this agent's own inbox.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.mailbox.clone()
    }

    /// Whether a detached worker may clone this emitter for a live tab — true
    /// only off [`FleetBus::session`], and read by `agent` to choose between
    /// [`Self::child`] and [`Self::muted_child`].
    pub(crate) fn is_session_lived(&self) -> bool {
        self.session_lived
    }

    /// The owning session's trace, likewise for `shell_eval::deferred_sink`: a
    /// deferred batch lands after the exchange, so it records through a durable
    /// file handle rather than a bus channel end.
    pub(crate) fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }
}

/// The event channel and its inbox, in one of two lifetimes: [`Self::session`],
/// held across the whole REPL session (the TUI), so a detached async child
/// streams to a live tab; or [`Self::per_exchange`], closing with the exchange
/// (headless, tests), so such a child stays muted on the display though it
/// still records.
pub(crate) struct FleetBus {
    tx: BusSender,
    rx: BusReceiver,
    mailbox: Mailbox,
    session_lived: bool,
    /// The one meter every emitter minted from this bus shares.
    meter: UsageMeter,
}

impl FleetBus {
    pub(crate) fn session(inbox: &Inbox) -> Self {
        Self::build(inbox.mailbox(), true)
    }

    pub(crate) fn per_exchange(inbox: &Inbox) -> Self {
        Self::build(inbox.mailbox(), false)
    }

    fn build(mailbox: Mailbox, session_lived: bool) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            mailbox,
            session_lived,
            meter: UsageMeter::default(),
        }
    }

    /// The receiver the exchange's `Sink` drains.
    pub(crate) fn rx(&self) -> &BusReceiver {
        &self.rx
    }

    /// The root emitter for this bus; children derive from it with
    /// [`Emitter::child`] / [`Emitter::muted_child`] and inherit the meter.
    pub(crate) fn emitter(&self, id: AgentId, transcript: Transcript) -> Emitter {
        Emitter {
            tx: self.tx.clone(),
            id,
            mailbox: self.mailbox.clone(),
            session_lived: self.session_lived,
            transcript,
            meter: self.meter.clone(),
        }
    }

    /// The run total across the root and every child — what headless reports.
    pub(crate) fn usage_total(&self) -> Usage {
        self.meter.total()
    }
}

#[cfg(test)]
pub(crate) fn dummy_emitter() -> (Emitter, BusReceiver) {
    let (tx, rx) = channel();
    (Emitter::new(tx, 0), rx)
}

#[cfg(test)]
mod tests {
    use super::{FleetBus, Inbox, Kind, Transcript};

    /// Accounting follows the event, not its emitter: no sink ever sees the
    /// muted child's `Usage`, yet the run total includes it.
    #[test]
    fn usage_meter_counts_a_muted_child_on_a_dead_channel() {
        use crate::provider::Usage;

        let root_usage = Usage {
            input: 100,
            output: 20,
            dollars: 0.5,
            ..Usage::default()
        };
        let child_usage = Usage {
            input: 7,
            output: 3,
            dollars: 0.125,
            ..Usage::default()
        };

        let bus = FleetBus::per_exchange(&Inbox::new());
        let root = bus.emitter(0, Transcript::none());
        let child = root.muted_child(1, Transcript::none());

        root.emit(Kind::Usage(root_usage));
        child.emit(Kind::Usage(child_usage));

        let total = bus.usage_total();
        assert_eq!(total.input, 107, "the muted child's input is counted");
        assert_eq!(total.output, 23, "the muted child's output is counted");
        assert!(
            (total.dollars - 0.625).abs() < f64::EPSILON,
            "the muted child's cost is counted",
        );
    }
}
