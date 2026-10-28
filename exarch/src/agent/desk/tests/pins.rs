//! The pins family, and the surfaces it records beside.

use super::*;

/// `` `exarch-pins `set ``: a one-span text card under `key`.
fn pin_set_req(key: &str, text: &str) -> Request {
    let body = FOValue::Map {
        entries: vec![(
            "spans".into(),
            FOValue::List {
                items: vec![FOValue::Map {
                    entries: vec![("text".into(), text.to_string().encode())],
                }],
            },
        )],
    };
    let body = Card::decode(&tag("text", Some(body))).expect("a one-span text card");
    Request::Pins(Pins::Set(Pin {
        key: key.to_string(),
        body,
    }))
}

fn pin_clear_req(key: &str) -> Request {
    Request::Pins(Pins::Clear(key.to_string()))
}

fn pin_read_req(key: &str) -> Request {
    Request::Pins(Pins::Read(key.to_string()))
}

/// What a read answered: the card of a `some`, or `None` for a `none`.
fn slot(answer: FOValue) -> Option<FOValue> {
    match answer {
        FOValue::Variant { label, payload } if label == "some" => payload.map(|card| *card),
        FOValue::Variant {
            label,
            payload: None,
        } if label == "none" => None,
        other => panic!("a read answers `some or `none, got {other:?}"),
    }
}

/// `` `set ``/`` `clear `` write and empty the register mirror
/// `` `read ``/`` `list `` answer from.
#[test]
fn pin_set_and_clear_round_trip_through_the_desk() {
    let (d, _agent) = desk();

    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "an unset key must read `none"
    );

    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");
    let answer = slot(
        d.ask(pin_read_req("tasks"))
            .expect("a set key must read back"),
    )
    .expect("a set key reads `some");
    let card = crate::card::value_to_card(&answer).expect("the readback must decode as a card");
    assert!(
        matches!(
            card.marks(),
            [crate::card::Mark::Text { spans }]
                if spans.len() == 1 && spans[0].text == "hi"
        ),
        "`set` must write the canonical card, got {card:?}"
    );

    d.ask(pin_clear_req("tasks"))
        .expect("`exarch-pins `clear` must answer Ok");
    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "`clear` must empty the slot `set` wrote"
    );
}

/// `` `exarch-pins `list `` names exactly the occupied keys, in
/// `BTreeMap` order, and tracks a set/clear pair.
#[test]
fn pin_list_tracks_set_and_clear() {
    let (d, _agent) = desk();
    let keys = |d: &ExarchDesk| match d
        .ask(Request::Pins(Pins::List))
        .expect("`exarch-pins `list` must answer Ok")
    {
        FOValue::List { items } => items
            .into_iter()
            .map(|v| match v {
                FOValue::String { value } => value,
                other => panic!("`exarch-pins `list` must answer strings, got {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("`exarch-pins `list` must answer a list, got {other:?}"),
    };

    assert!(keys(&d).is_empty(), "an empty register lists no keys");

    d.ask(pin_set_req("b", "one"))
        .expect("`exarch-pins `set` must answer Ok");
    d.ask(pin_set_req("a", "two"))
        .expect("`exarch-pins `set` must answer Ok");
    assert_eq!(
        keys(&d),
        vec!["a", "b"],
        "keys list in BTreeMap (lexicographic) order"
    );

    d.ask(pin_clear_req("b"))
        .expect("`exarch-pins `clear` must answer Ok");
    assert_eq!(keys(&d), vec!["a"], "a clear drops its key from the list");
}

/// Every surface class the live applier renders also records through the
/// seam — a `Display` record (or, for the pin register, `Forensic`) — in
/// the order it arrived.  A pin/unpin additionally publishes a `Transient`
/// beside its `Forensic` twin: the durable breadcrumb and the live
/// register the process is holding, two records for one act.
#[test]
fn live_surfaces_record_their_seam_twins() {
    use crate::bus::Signal;
    use crate::record::{Display, Forensic, Record, Transient};

    let (d, _agent) = desk();
    let (tx, rx) = channel();
    d.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(crate::bus::FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    let applier = SurfaceApplier::new(d.services.log.borrow().record_emitter());

    let read = ral_core::shell::runtime::Observation::instant(
        None,
        None,
        ral_core::shell::runtime::Observed::Read(ral_core::fact::Read {
            path: "a.rs".into(),
        }),
    );
    applier.surface(&read.to_surface());
    applier.surface(&FOValue::Variant {
        label: "card".into(),
        payload: Some(Box::new(FOValue::List { items: vec![] })),
    });
    applier.surface(
        &ral_core::shell::runtime::DoneEvent {
            id: ral_core::shell::runtime::WorkerId(7),
            title: "<block>".into(),
            elapsed_ms: 0,
            outcome: ral_core::shell::runtime::Done::Ok,
        }
        .to_surface(),
    );
    applier.surface(
        &ral_core::shell::runtime::Notice::Reap(ral_core::shell::runtime::ReapNotice {
            id: ral_core::shell::runtime::WorkerId(1),
            title: "sleep 10".into(),
            cause: ral_core::shell::runtime::ReapCause::Retention,
        })
        .to_surface(),
    );
    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");
    d.ask(pin_clear_req("tasks"))
        .expect("`exarch-pins `clear` must answer Ok");

    let mut facts: Vec<&'static str> = Vec::new();
    let mut transients: Vec<&'static str> = Vec::new();
    while let Ok(sig) = rx.try_recv() {
        match sig {
            Signal::Fact(_, fact) => facts.push(match fact.value() {
                Record::Display(Display::Observation { .. }) => "io",
                Record::Display(Display::Card { .. }) => "card",
                Record::Display(Display::Done {
                    outcome: crate::card::DoneOutcome::Ok,
                    ..
                }) => "done",
                Record::Forensic(Forensic::Reap { cause, .. }) if cause == "retention" => "notice",
                Record::Forensic(Forensic::Pin { key }) if key == "tasks" => "pin",
                Record::Forensic(Forensic::Unpin { key }) if key == "tasks" => "unpin",
                _ => continue,
            }),
            Signal::Transient(_, t) => transients.push(match t {
                Transient::Pin { key, .. } if key == "tasks" => "pin",
                Transient::Unpin { key } if key == "tasks" => "unpin",
                _ => continue,
            }),
            Signal::Born(..) => {}
        }
    }
    assert_eq!(
        transients,
        ["pin", "unpin"],
        "the pin register's live copy also publishes as a Transient beside its Forensic twin"
    );
    assert_eq!(
        facts,
        ["io", "card", "done", "notice", "pin", "unpin"],
        "every class records its twin, in the order it surfaced"
    );
}
