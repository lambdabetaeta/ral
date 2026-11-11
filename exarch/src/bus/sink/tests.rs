use super::Sink;
use crate::bus::FleetBus;
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

    fn synced(&mut self, id: AgentId, through: u64) {
        self.0.push((id, format!("synced {through}")));
    }

    fn transient(&mut self, id: AgentId, transient: &Transient) {
        assert!(matches!(transient, Transient::Boundary));
        self.0.push((id, "boundary".into()));
    }
}

#[test]
fn accept_routes_birth_sync_commit_records_and_transient_in_feed_order() {
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
            (id, "synced 0".into()),
            (id, "first".into()),
            (id, "second".into()),
            (id, "boundary".into()),
        ]
    );
}
