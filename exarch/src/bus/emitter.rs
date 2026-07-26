//! The producing end of the bus: the handle a worker stamps its events
//! through ([`Emitter`]), the run-scoped accounting every emitter tees to
//! ([`UsageMeter`]), and the object that owns the channel and mints
//! emitters for a session or a single exchange ([`FleetBus`]).

use super::AgentId;
use super::channel::{BusReceiver, BusSender, channel};
use super::event::{Event, Kind};
use super::inbox::{Inbox, Mailbox};
use crate::agent::transcript::Transcript;
use crate::provider::Usage;
use crate::sync::LockExt;
use std::sync::{Arc, Mutex};

/// A run-scoped usage accumulator.
///
/// Where a [`Transcript`] is **per-session**
/// — each session records its own trace — this meter is **per-run**: the root
/// and every child, muted or live, share the single instance the
/// [`FleetBus`] mints.  That shared lifetime is the whole point: an async
/// sub-agent's display channel is dead in headless, so its usage never reaches
/// a sink, but its `emit` still tees here, so the run total counts it.
/// Accounting follows the event, not its emitter, exactly as recording does.
#[derive(Clone, Default)]
pub(crate) struct UsageMeter(Arc<Mutex<Usage>>);

impl UsageMeter {
    /// Fold one usage delta into the run total.
    pub(crate) fn add(&self, u: Usage) {
        *self.0.lock_ignore_poison() += u;
    }

    /// The run total so far.
    pub(crate) fn total(&self) -> Usage {
        *self.0.lock_ignore_poison()
    }
}

#[derive(Clone)]
pub struct Emitter {
    tx: BusSender,
    id: AgentId,
    /// The **owning** session's mailbox — root's for the root emitter, the
    /// child's own for a child emitter.  Used by the `spawn` boundary sink to
    /// post a deferred surface batch into the agent that ran the spawn.  An
    /// emitter never carries another agent's mailbox.
    mailbox: Mailbox,
    /// Whether this emitter's channel outlives the spawning exchange, so a
    /// *detached* worker (an async `agent` child) may clone it for a live
    /// tab.  The TUI's session-lived bus sets it; headless's per-exchange bus
    /// leaves it `false`, keeping async children muted *on the display* —
    /// bus lifetime is a TUI property, not a core obligation.  It does not
    /// gate [`Self::transcript`]: a muted child still records its own trace.
    session_lived: bool,
    /// This emitter's owning session's [`Transcript`].  Every [`Self::emit`]
    /// tees here, so the session's operational trace is written at the emit
    /// seam — independent of who drains the live bus for display, and so a
    /// child muted off a per-exchange bus still records its full trace.
    transcript: Transcript,
    /// The run's [`UsageMeter`], shared by the root and every child.  Every
    /// [`Self::emit`] of a [`crate::bus::Kind::Usage`] tees here too, so the run total is
    /// accumulated at the emit seam regardless of display muting — the same
    /// reasoning that puts the transcript record here, but per-run not
    /// per-session.
    meter: UsageMeter,
}

impl Emitter {
    /// An emitter with a standalone, orphan mailbox and no transcript — for
    /// tests, whose events land nowhere durable.
    pub fn new(tx: BusSender, id: AgentId) -> Self {
        Self::with_mailbox(tx, id, Inbox::new().mailbox())
    }

    /// [`Self::new`] with a caller-supplied mailbox instead of a throwaway
    /// one — what the crate's own unit tests use to wire a session's real
    /// inbox through.
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

    /// A muted child emitter derived from this (parent) emitter: a dead display
    /// channel and an orphan mailbox, but a live [`Transcript`] *and* the
    /// parent run's [`UsageMeter`].  The headless async child takes this — it
    /// streams nowhere (its receiver is already dropped) yet still records its
    /// own operational trace and tees its usage to the inherited run meter,
    /// because recording and accounting are run properties, not display ones.
    /// Display-muted, accounting-live.
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

    /// A sibling emitter for a child session: the same event channel and
    /// session-lived flag, stamped with the child's id, carrying the
    /// **child's own** mailbox and **own** [`Transcript`] but the **shared**
    /// run [`UsageMeter`] — so the child's surface batches land in the child's
    /// box and its events in the child's trace, never the parent's, while its
    /// usage still folds into the one run total.
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

    /// Record `kind` to this session's [`Transcript`], fold it into the run
    /// [`UsageMeter`] if it is a [`crate::bus::Kind::Usage`], then send it — in that
    /// order, so recording and accounting never depend on a live receiver
    /// ([`Self::muted_child`]'s whole point).
    pub(crate) fn emit(&self, kind: Kind) {
        self.transcript.record(self.id, &kind);
        if let Kind::Usage(u) = &kind {
            self.meter.add(*u);
        }
        let _ = self.tx.send(Event { id: self.id, kind });
    }

    /// The owning session's mailbox, for the `spawn` boundary sink that posts
    /// a deferred surface batch back into this agent's own inbox.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.mailbox.clone()
    }

    /// Whether a detached worker may clone this emitter for a live tab.
    /// True only off a session-lived bus ([`FleetBus::session`]); an async
    /// `agent` reads it to choose a streaming tab over its muted log.
    pub(crate) fn is_session_lived(&self) -> bool {
        self.session_lived
    }

    /// This emitter's owning session's [`Transcript`] — for a deferred
    /// callback (a spawn worker's boundary sink) that must outlive the exchange
    /// and so cannot hold a clone of this emitter itself: a `Transcript` is
    /// a durable file handle, not a bus channel end, so holding one long
    /// past this exchange never keeps a `pump`/`drive` completion waiting on a
    /// sender that will not drop (the daemon-task-hang class of bug
    /// [`crate::bus::drain_pass`]'s doc already guards against).
    pub(crate) fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }
}

/// The event channel and its inbox, owned for as long as the host wants a
/// worker→frontend bus to live.  Two lifetimes, one type:
///
/// - [`Self::session`] — minted once at REPL start and held for the whole
///   session.  Each exchange's foreground worker and every detached async child
///   clone its sender (session-lived, so a background child gets a live tab);
///   the idle wait drains it as a third source.
/// - [`Self::per_exchange`] — minted fresh for one exchange (headless, tests), so the
///   channel closes when the exchange's worker finishes.  Its emitters are *not*
///   session-lived: an async child stays muted *on the display* (it never
///   streams to a live tab) — the observable display behaviour headless has
///   always had.  It still records its own `transcript.jsonl`, since recording
///   rides the emitter, not the channel's lifetime.
///
/// Either way [`crate::bus::pump`] borrows the channel — completion is the per-exchange
/// `done` flag, never the channel's lifetime.
pub(crate) struct FleetBus {
    tx: BusSender,
    rx: BusReceiver,
    mailbox: Mailbox,
    session_lived: bool,
    /// The one run-scoped [`UsageMeter`] every emitter minted from this bus
    /// shares — the root through [`Self::emitter`], each child through
    /// [`Emitter::child`] / [`Emitter::muted_child`].  [`Self::usage_total`]
    /// reads it for the run total, independent of which sink (if any) drains
    /// the bus.
    meter: UsageMeter,
}

impl FleetBus {
    /// A session-lived bus over `inbox` (the TUI root's inbox).  Emitters
    /// minted from it are session-lived, so detached async children stream.
    pub(crate) fn session(inbox: &Inbox) -> Self {
        Self::build(inbox.mailbox(), true)
    }

    /// A per-exchange bus over `inbox` (headless / tests).  Emitters are not
    /// session-lived, so async children stay muted on the display (they still
    /// record their own trace).
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

    /// The receiver the exchange's [`crate::bus::Sink`] drains.
    pub(crate) fn rx(&self) -> &BusReceiver {
        &self.rx
    }

    /// An [`Emitter`] stamped with `id`, sharing this bus's sender, root
    /// mailbox, session-lived flag, and run [`UsageMeter`].  The root attend
    /// thread takes one; a child emitter is derived with [`Emitter::child`] /
    /// [`Emitter::muted_child`], inheriting the same meter.
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

    /// The run's total usage so far, summed across the root and every child at
    /// the emit seam — the single source of truth for the headless result,
    /// independent of display muting.
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

    /// The run meter counts a muted child's usage.  Accounting follows the
    /// event, not its emitter: a muted child's display channel is dead (its
    /// receiver dropped, so a sink never sees its [`crate::bus::Kind::Usage`]), yet it shares
    /// the root's run meter through [`Emitter::muted_child`], so `bus.usage_total()` sums
    /// the root *and* the muted child — exactly the headless under-reporting
    /// this fixes.
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
        // The muted child: a fresh dead channel, but the root run's meter.
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
