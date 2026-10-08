use super::{Sink, pump};
use crate::bus::{Emitter, FleetBus, Inbox};
use crate::record::AgentId;
use crate::record::{Record, Recorded, Transient};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `pump` returns on the worker's `done` while a holder keeps an `Emitter`
/// clone — a live sender — past the worker's return, as a `spawn`ed server
/// that never terminates would.
#[test]
fn pump_returns_on_worker_done_not_sender_disconnect() {
    struct CountSink(usize);
    impl Sink for CountSink {
        fn transient(&mut self, _id: AgentId, _t: &Transient) {
            self.0 += 1;
        }
    }

    let mut sink = CountSink(0);
    let bus = FleetBus::session(&Inbox::new());
    // Outlives `pump`, holding an `Emitter` clone whose sender keeps the
    // channel from ever disconnecting.
    let holder: Mutex<Option<Emitter>> = Mutex::new(None);

    let recorder = crate::record::Emitter::none();
    recorder.attach(bus.emitter(AgentId::new(0)).fleet_sink());

    let t0 = Instant::now();
    let r = pump(&mut sink, &bus, AgentId::new(0), &recorder, |emit| {
        *holder.lock().unwrap() = Some(emit.clone());
        recorder.transient(Transient::Boundary);
        "done"
    })
    .expect("pump returns Ok");

    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "pump must return on the explicit done signal, not wait for sender disconnect (took {:?})",
        t0.elapsed()
    );
    assert_eq!(r, Some("done"), "pump returns the worker's value");
    assert_eq!(sink.0, 1, "the worker's one delta was delivered");
    assert!(holder.lock().unwrap().is_some());
}

/// A recovered worker panic records a `Forensic::Error` through the seam
/// rather than vanishing into the join, and `pump` reports `None`.
#[test]
fn recovered_panic_records_a_forensic_error() {
    struct FactSink(Vec<String>);
    impl Sink for FactSink {
        fn fact(&mut self, _id: AgentId, rec: &Recorded<Record>) {
            if let Record::Forensic(crate::record::Forensic::Error { text }) = rec.value() {
                self.0.push(text.clone());
            }
        }
    }

    let mut sink = FactSink(Vec::new());
    let bus = FleetBus::session(&Inbox::new());
    let recorder = crate::record::Emitter::none();
    recorder.attach(bus.emitter(AgentId::new(0)).fleet_sink());

    let r = pump(&mut sink, &bus, AgentId::new(0), &recorder, |_emit| {
        panic!("boom")
    })
    .expect("pump returns Ok even when the worker panics");

    assert_eq!(r, None, "a panicked worker yields no value");
    assert!(
        sink.0
            .iter()
            .any(|text| text.starts_with(crate::bus::WORKER_PANIC_PREFIX) && text.contains("boom")),
        "the panic reaches the seam as a Forensic::Error, got {:?}",
        sink.0
    );
}
