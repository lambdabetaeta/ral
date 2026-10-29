use super::{Sink, panic_msg};
use crate::bus::{FleetBus, Signal};
use crate::record::{AgentId, Born, Forensic, NonEmpty, Record, Recorded, Transient};

#[derive(Default)]
struct Trace(Vec<(AgentId, String)>);

impl Sink for Trace {
    fn born(&mut self, id: AgentId, born: &Born) {
        self.0.push((id, format!("born {}", born.name)));
    }

    fn fact(&mut self, id: AgentId, record: &Recorded<Record>) {
        let Record::Forensic(Forensic::SystemNote { text }) = record.value() else {
            panic!("the test commits only system notes")
        };
        self.0.push((id, text.clone()));
    }

    fn transient(&mut self, id: AgentId, transient: &Transient) {
        assert!(matches!(transient, Transient::Boundary));
        self.0.push((id, "boundary".into()));
    }
}

#[test]
fn accept_routes_birth_commit_records_and_transient_in_feed_order() {
    let bus = FleetBus::session();
    let id = AgentId::new(1);
    let recorder = crate::record::Emitter::none();
    let dir = ral_core::own::OwnDir::for_test("sink-birth").unwrap();
    let child = bus.emitter(AgentId::new(0)).child(
        id,
        Born {
            log_dir: (*dir).clone(),
            name: "helper".into(),
            parent: Some(AgentId::new(0)),
            resumed: None,
            reader: recorder.reader(),
        },
    );
    recorder.attach(child.fleet_sink());
    let _commit = recorder
        .commit(NonEmpty {
            first: Forensic::SystemNote {
                text: "first".into(),
            }
            .into(),
            rest: vec![
                Forensic::SystemNote {
                    text: "second".into(),
                }
                .into(),
            ],
        })
        .unwrap();
    recorder.transient(Transient::Boundary);

    let mut trace = Trace::default();
    while let Ok(signal) = bus.rx().try_recv() {
        trace.accept(signal);
    }

    assert_eq!(
        trace.0,
        [
            (id, "born helper".into()),
            (id, "first".into()),
            (id, "second".into()),
            (id, "boundary".into()),
        ]
    );
}

#[test]
fn drain_records_keeps_committed_order_and_excludes_transients() {
    let bus = FleetBus::session();
    let recorder = crate::record::Emitter::none();
    recorder.attach(bus.emitter(AgentId::new(0)).fleet_sink());
    recorder.transient(Transient::Boundary);
    for text in ["first", "second"] {
        let _recorded = recorder
            .emit(Forensic::SystemNote { text: text.into() })
            .unwrap();
    }
    recorder.transient(Transient::Died { failures: vec![] });

    let records = super::drain_records(bus.rx());

    assert!(matches!(records.as_slice(), [
        Record::Forensic(Forensic::SystemNote { text: first }),
        Record::Forensic(Forensic::SystemNote { text: second }),
    ] if first == "first" && second == "second"));
    assert!(bus.rx().try_recv().is_err());
}

#[test]
fn panic_messages_preserve_both_string_payloads_and_name_other_payloads() {
    let borrowed: Box<dyn std::any::Any + Send> = Box::new("borrowed panic");
    let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned panic"));
    let other: Box<dyn std::any::Any + Send> = Box::new(17_u32);
    assert_eq!(panic_msg(&borrowed), "borrowed panic");
    assert_eq!(panic_msg(&owned), "owned panic");
    assert_eq!(panic_msg(&other), "non-string payload");
}

#[test]
fn a_sink_may_ignore_signals_it_does_not_present() {
    struct Silent;
    impl Sink for Silent {}
    Silent.accept(Signal::Transient(
        AgentId::new(0),
        Transient::Died { failures: vec![] },
    ));
}
