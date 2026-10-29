//! The producing end of the bus: [`Emitter`], the handle a worker stamps its
//! events through, and [`FleetBus`], which owns the channel and mints emitters.

use super::channel::{BusReceiver, BusSender, WeakSender, channel};
use super::signal::Signal;
use crate::provider::Usage;
use crate::record::AgentId;
use crate::record::{Born, Commit, Forensic, Publish, Record, Transient};
use ral_core::sync::LockExt;
use std::sync::{Arc, Mutex};

/// A usage accumulator shared by every emitter of one run — where a
/// session's record log is per-session, this is per-run. Usage tees here at
/// the record seam, independently of the frontend.
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
/// meter; accounting follows the fact even when the channel has no receiver.
pub(crate) struct FleetSink {
    pub(crate) id: AgentId,
    /// Weak on purpose: the log outlives any one bus and its facts are
    /// durable without one, so this handle must never hold a channel open or
    /// stall a drain's disconnect on a session object's lifetime.
    pub(crate) tx: WeakSender,
    pub(crate) meter: UsageMeter,
}

impl Publish for FleetSink {
    fn commit(&self, commit: Commit) {
        for recorded in commit.records() {
            if let Record::Forensic(Forensic::UsageDelta { usage, .. }) = recorded.value() {
                self.meter.add(*usage);
            }
        }
        if self
            .tx
            .send_signal(Signal::Commit(self.id, commit))
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

#[derive(Clone)]
pub struct Emitter {
    tx: BusSender,
    id: AgentId,
    meter: UsageMeter,
}

impl Emitter {
    pub fn new(tx: BusSender, id: AgentId) -> Self {
        Self {
            tx,
            id,
            meter: UsageMeter::default(),
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
            meter: self.meter.clone(),
        }
    }

    /// Where a session's record log publishes: this emitter's channel and run
    /// meter, tagged with its agent id — what `Agent::couple` attaches to the
    /// seam at every point a session meets a live bus.
    pub(crate) fn fleet_sink(&self) -> Box<dyn Publish> {
        Box::new(FleetSink {
            id: self.id,
            tx: self.tx.downgrade(),
            meter: self.meter.clone(),
        })
    }
}

/// One fleet-wide channel and usage meter, spanning every job.
pub(crate) struct FleetBus {
    tx: BusSender,
    rx: BusReceiver,
    /// The one meter every emitter minted from this bus shares.
    meter: UsageMeter,
}

impl FleetBus {
    pub(crate) fn session() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            meter: UsageMeter::default(),
        }
    }

    /// The receiver the frontend drains.
    pub(crate) fn rx(&self) -> &BusReceiver {
        &self.rx
    }

    /// A root emitter; children inherit its run meter.
    pub(crate) fn emitter(&self, id: AgentId) -> Emitter {
        Emitter {
            tx: self.tx.clone(),
            id,
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
    use crate::bus::Signal;
    use crate::provider::Usage;
    use crate::record::{AgentId, Born, Emitter as Recorder, Forensic};
    use ral_core::own::OwnDir;

    #[test]
    fn session_meters_root_and_child_before_and_after_channel_closure() {
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

        let bus = FleetBus::session();
        let root_id = AgentId::new(0);
        let child_id = AgentId::new(1);
        let root = bus.emitter(root_id);
        let root_seam = Recorder::none();
        root_seam.attach(root.fleet_sink());
        let child_seam = Recorder::none();
        let dir = OwnDir::for_test("emitter-child").expect("child log directory");
        let child = root.child(
            child_id,
            Born {
                log_dir: (*dir).clone(),
                name: "helper".into(),
                parent: Some(root_id),
                resumed: None,
                recorder: child_seam.clone(),
            },
        );
        child_seam.attach(child.fleet_sink());
        let record_usage = || {
            for (seam, usage) in [(&root_seam, root_usage), (&child_seam, child_usage)] {
                let _recorded = seam
                    .emit(Forensic::UsageDelta {
                        usage,
                        generation: None,
                        provider: None,
                    })
                    .expect("a fileless seam still stamps");
            }
        };

        record_usage();

        let signals: Vec<_> = std::iter::from_fn(|| bus.rx().try_recv().ok()).collect();
        assert!(matches!(signals.as_slice(), [
            Signal::Born(born_id, born),
            Signal::Commit(committing_root, _),
            Signal::Commit(committing_child, _),
        ] if *born_id == child_id && born.name == "helper" && born.parent == Some(root_id)
            && *committing_root == root_id && *committing_child == child_id));
        let total = bus.usage_total();
        assert_eq!(total.input, 107);
        assert_eq!(total.output, 23);
        assert!((total.dollars - 0.625).abs() < f64::EPSILON);

        let meter = bus.meter();
        drop(bus);
        drop(root);
        drop(child);
        record_usage();

        let total = meter.total();
        assert_eq!(
            total.input, 214,
            "both record seams meter without a channel"
        );
        assert_eq!(total.output, 46);
        assert!((total.dollars - 1.25).abs() < f64::EPSILON);
    }
}
