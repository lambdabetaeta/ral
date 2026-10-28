//! The producing end of the bus: [`Emitter`], the handle a worker stamps its
//! events through, and [`FleetBus`], which owns the channel and mints emitters.

use super::channel::{BusReceiver, BusSender, WeakSender, channel};
use super::signal::Signal;
use crate::provider::Usage;
use crate::record::AgentId;
use crate::record::{Born, Forensic, Publish, Record, Recorded, Transient};
use ral_core::sync::LockExt;
use std::sync::{Arc, Mutex};

/// A usage accumulator shared by every emitter of one run — where a
/// session's record log is per-session, this is per-run.  Usage tees here at
/// the emit seam, so a display-muted child whose events reach no sink still
/// counts toward the total.
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

/// Where a witnessed record goes beside the file: the fleet-wide channel a
/// frontend drains, tagged with the owning session's id, and the run's usage
/// meter; accounting follows the fact through the seam, so a display-muted
/// child on a dead channel still counts toward the run total.
pub(crate) struct FleetSink {
    pub(crate) id: AgentId,
    /// Weak on purpose: the log outlives any one bus and its facts are
    /// durable without one, so this handle must never hold a channel open or
    /// stall a drain's disconnect on a session object's lifetime.
    pub(crate) tx: WeakSender,
    pub(crate) meter: UsageMeter,
}

impl Publish for FleetSink {
    fn fact(&self, recorded: Recorded<Record>) {
        if let Record::Forensic(Forensic::UsageDelta { usage, .. }) = recorded.value() {
            self.meter.add(*usage);
        }
        if self
            .tx
            .send_signal(Signal::Fact(self.id, recorded))
            .is_err()
        {
            // No live receiver; the record is already durable on disk, which is
            // the whole point: a pressured or absent consumer catches up from the
            // file, never from the channel.
        }
    }

    fn transient(&self, t: Transient) {
        if self.tx.send_signal(Signal::Transient(self.id, t)).is_err() {
            // No live receiver; a transient has no durable form to catch up
            // from, so there is nothing else to do.
        }
    }
}

/// Whether an async `agent` child (a worker) cloning this emitter gets a live
/// tab or streams nowhere.  Display only: a [`Children::Muted`] child still
/// records through its own record seam and still meters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Children {
    Muted,
    Live,
}

#[derive(Clone)]
pub struct Emitter {
    tx: BusSender,
    id: AgentId,
    children: Children,
    meter: UsageMeter,
}

impl Emitter {
    pub fn new(tx: BusSender, id: AgentId) -> Self {
        Self {
            tx,
            id,
            children: Children::Muted,
            meter: UsageMeter::default(),
        }
    }

    /// A child emitter onto a channel whose receiver is already dropped, so it
    /// streams nowhere, but carrying the parent run's meter.  What an async
    /// `agent` child takes off a bus whose children are [`Children::Muted`].
    pub(crate) fn muted_child(&self, id: AgentId) -> Self {
        let (tx, _rx) = channel();
        Self {
            tx,
            id,
            children: Children::Muted,
            meter: self.meter.clone(),
        }
    }

    /// A child emitter sharing the channel and run meter. Its birth precedes
    /// every signal it can stamp.
    pub(crate) fn child(&self, id: AgentId, born: Born) -> Self {
        if self.tx.send_signal(Signal::Born(id, born)).is_err() {
            // A dead channel: the birth goes nowhere, as all that follows would.
        }
        Self {
            tx: self.tx.clone(),
            id,
            children: self.children,
            meter: self.meter.clone(),
        }
    }

    /// Where a session's record log publishes: this emitter's channel and run
    /// meter, tagged with its agent id — what `Avatar::couple` attaches to the
    /// seam at every point a session meets a live bus.
    pub(crate) fn fleet_sink(&self) -> Box<dyn Publish> {
        Box::new(FleetSink {
            id: self.id,
            tx: self.tx.downgrade(),
            meter: self.meter.clone(),
        })
    }

    /// This emitter's own agent id, for `shell_eval::deferred_sink`, which
    /// stamps a spawn worker's surface batch with the owning session's id.
    pub(crate) fn id(&self) -> AgentId {
        self.id
    }

    /// Whether a worker inherits the live session bus through [`Self::child`]
    /// or records off the display through [`Self::muted_child`].
    pub(crate) fn spawns_live_children(&self) -> bool {
        self.children == Children::Live
    }
}

/// A session bus keeps leased helpers connected across exchanges;
/// a per-exchange bus mutes children while retaining their recorded usage.
pub(crate) struct FleetBus {
    tx: BusSender,
    rx: BusReceiver,
    children: Children,
    /// The one meter every emitter minted from this bus shares.
    meter: UsageMeter,
}

impl FleetBus {
    pub(crate) fn session() -> Self {
        Self::build(Children::Live)
    }

    pub(crate) fn per_exchange() -> Self {
        Self::build(Children::Muted)
    }

    fn build(children: Children) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            children,
            meter: UsageMeter::default(),
        }
    }

    /// The receiver the exchange's `Sink` drains.
    pub(crate) fn rx(&self) -> &BusReceiver {
        &self.rx
    }

    /// A root emitter; children inherit its run meter.
    pub(crate) fn emitter(&self, id: AgentId) -> Emitter {
        Emitter {
            tx: self.tx.clone(),
            id,
            children: self.children,
            meter: self.meter.clone(),
        }
    }

    /// The run total across the root and every child — what headless reports.
    pub(crate) fn usage_total(&self) -> Usage {
        self.meter.total()
    }

    /// The meter itself, for a frontend that reads the total as it grows.
    pub(crate) fn meter(&self) -> UsageMeter {
        self.meter.clone()
    }
}

#[cfg(test)]
pub(crate) fn dummy_emitter() -> (Emitter, BusReceiver) {
    let (tx, rx) = channel();
    (Emitter::new(tx, AgentId::new(0)), rx)
}

#[cfg(test)]
mod tests {
    use super::FleetBus;

    /// Accounting follows the fact through the record seam, not its emitter:
    /// no sink ever sees the muted child's usage, yet the run total includes
    /// it, because a coupled log meters through the sink whatever the state
    /// of the channel it publishes on.
    #[test]
    fn usage_meter_counts_a_muted_child_on_a_dead_channel() {
        use crate::provider::Usage;
        use crate::record::{AgentId, Emitter as Recorder, Forensic};

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

        let bus = FleetBus::per_exchange();
        let root = bus.emitter(AgentId::new(0));
        let child = root.muted_child(AgentId::new(1));

        let root_seam = Recorder::none();
        root_seam.attach(root.fleet_sink());
        let child_seam = Recorder::none();
        child_seam.attach(child.fleet_sink());

        let _recorded = root_seam
            .emit(Forensic::UsageDelta {
                usage: root_usage,
                generation: None,
                provider: None,
            })
            .expect("a fileless seam still stamps");
        let _recorded = child_seam
            .emit(Forensic::UsageDelta {
                usage: child_usage,
                generation: None,
                provider: None,
            })
            .expect("a fileless seam still stamps");

        let total = bus.usage_total();
        assert_eq!(total.input, 107, "the muted child's input is counted");
        assert_eq!(total.output, 23, "the muted child's output is counted");
        assert!(
            (total.dollars - 0.625).abs() < f64::EPSILON,
            "the muted child's cost is counted",
        );
    }
}
