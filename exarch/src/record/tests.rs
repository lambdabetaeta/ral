use super::*;
use serde_json::json;

fn reply() -> FOValue {
    FOValue::Map {
        entries: vec![(
            "answer".into(),
            FOValue::List {
                items: vec![
                    FOValue::Int { value: 42 },
                    FOValue::String {
                        value: "a reply\nwith a second line".into(),
                    },
                ],
            },
        )],
    }
}

#[test]
fn fold_of_no_status_is_working() {
    assert_eq!(ChildStatus::fold([]), ChildStatus::Working);
    let records = [
        Protocol::Cleared.into(),
        Display::Answer {
            text: "an answer is not a status".into(),
        }
        .into(),
        Forensic::SessionEnded.into(),
    ];
    assert_eq!(ChildStatus::fold(&records), ChildStatus::Working);
}

#[test]
fn fold_is_the_last_status_record() {
    let mut records = Vec::new();
    for status in [
        ChildStatus::Replied(reply()),
        ChildStatus::Working,
        ChildStatus::Failed("ended before reporting".into()),
    ] {
        records.extend([
            Protocol::Cleared.into(),
            Forensic::Status {
                status: status.clone(),
            }
            .into(),
            Display::Answer {
                text: "not a status".into(),
            }
            .into(),
            Forensic::SystemNote {
                text: "also not a status".into(),
            }
            .into(),
        ]);
        assert_eq!(ChildStatus::fold(&records), status);
    }
}

#[test]
fn a_status_record_round_trips() {
    for (status, wire_status) in [
        (ChildStatus::Working, json!("working")),
        (ChildStatus::Replied(reply()), json!({ "replied": reply() })),
        (
            ChildStatus::Failed("cancelled".into()),
            json!({ "failed": "cancelled" }),
        ),
    ] {
        let record = Forensic::Status {
            status: status.clone(),
        }
        .into();
        let line = commit_line(std::slice::from_ref(&record));
        let wire: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            wire["commit"]["records"][0]["Forensic"],
            json!({ "status": { "status": wire_status } })
        );
        let Some((_, records)) = parse_line(&line) else {
            panic!("a status record's line must unseal");
        };
        let [Record::Forensic(Forensic::Status { status: back })] = records.as_slice() else {
            panic!("a status record must round-trip as a status record");
        };
        assert_eq!(back, &status);
    }
}

#[test]
fn settlement_drops_the_value() {
    assert_eq!(ChildStatus::Working.settlement(), None);
    assert_eq!(
        ChildStatus::Replied(reply()).settlement(),
        Some(Settlement::Replied)
    );
    assert_eq!(
        ChildStatus::Failed("cancelled".into()).settlement(),
        Some(Settlement::Failed {
            reason: "cancelled".into(),
        })
    );
}
