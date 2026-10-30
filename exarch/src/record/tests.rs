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
fn a_status_record_round_trips() {
    for (status, wire_status) in [
        (ChildStatus::Stopped, json!("stopped")),
        (ChildStatus::Replied(reply()), json!({ "replied": reply() })),
        (
            ChildStatus::Failed("cancelled".into()),
            json!({ "failed": "cancelled" }),
        ),
    ] {
        let record = Record::Control(Control::Status {
            status: status.clone(),
        });
        let line = commit_line(std::slice::from_ref(&record));
        let wire: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            wire["commit"]["records"][0]["Control"],
            json!({ "status": { "status": wire_status } })
        );
        let Some((_, records)) = parse_line(&line) else {
            panic!("a status record's line must unseal");
        };
        let [Record::Control(Control::Status { status: back })] = records.as_slice() else {
            panic!("a status record must round-trip as a status record");
        };
        assert_eq!(back, &status);
    }
}

#[test]
fn settlement_drops_the_value() {
    assert_eq!(
        ChildStatus::Replied(reply()).settlement(),
        Settlement::Replied
    );
    assert_eq!(
        ChildStatus::Failed("cancelled".into()).settlement(),
        Settlement::Failed {
            reason: "cancelled".into(),
        }
    );
    assert_eq!(ChildStatus::Stopped.settlement(), Settlement::Stopped);
}

#[test]
fn a_stopped_settlement_round_trips() {
    let outcome = ChildStatus::Stopped.settlement();
    let wire = serde_json::to_value(&outcome).unwrap();
    assert_eq!(wire, json!({ "outcome": "stopped" }));
    assert_eq!(serde_json::from_value::<Settlement>(wire).unwrap(), outcome);
}

#[test]
fn a_job_id_is_its_opening_commit_number() {
    for commit in [0, 42, u64::MAX] {
        let job = JobId::from(commit);
        assert_eq!(job.get(), commit);
        let wire = serde_json::to_value(job).unwrap();
        assert_eq!(wire, json!(commit));
        assert_eq!(serde_json::from_value::<JobId>(wire).unwrap(), job);
    }
}

#[test]
fn job_and_engine_forensics_round_trip() {
    use ral_core::carrier::Severed;
    use std::time::Duration;

    for (forensic, wire_forensic) in [
        (Record::Control(Control::JobOpened), json!("job_opened")),
        (
            Record::Control(Control::JobSettled {
                job: JobId::from(42),
                stopped: false,
            }),
            json!({ "job_settled": { "job": 42, "stopped": false } }),
        ),
        (
            Record::Control(Control::JobSettled {
                job: JobId::from(42),
                stopped: true,
            }),
            json!({ "job_settled": { "job": 42, "stopped": true } }),
        ),
        (
            Record::Forensic(Forensic::EngineSevered {
                cause: Severed::Refused("version mismatch".into()),
            }),
            json!({ "engine_severed": { "cause": { "Refused": "version mismatch" } } }),
        ),
        (
            Record::Forensic(Forensic::EngineSevered {
                cause: Severed::Closed("unexpected EOF".into()),
            }),
            json!({ "engine_severed": { "cause": { "Closed": "unexpected EOF" } } }),
        ),
        (
            Record::Forensic(Forensic::EngineSevered {
                cause: Severed::Silent(Duration::from_millis(1500)),
            }),
            json!({ "engine_severed": {
                "cause": { "Silent": { "secs": 1, "nanos": 500_000_000 } }
            } }),
        ),
        (
            Record::Forensic(Forensic::EngineSevered {
                cause: Severed::Faulted("invalid frame".into()),
            }),
            json!({ "engine_severed": { "cause": { "Faulted": "invalid frame" } } }),
        ),
        (
            Record::Forensic(Forensic::TeardownFailed {
                failure: "engine end was not certified".into(),
            }),
            json!({ "teardown_failed": { "failure": "engine end was not certified" } }),
        ),
    ] {
        let record = forensic;
        let line = commit_line(std::slice::from_ref(&record));
        let wire: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            wire["commit"]["records"][0]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap(),
            &wire_forensic
        );
        let (_, records) = parse_line(&line).expect("a forensic record's line must unseal");
        let [back] = records.as_slice() else {
            panic!("a forensic record must round-trip as a forensic record");
        };
        assert_eq!(
            serde_json::to_value(back)
                .unwrap()
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap(),
            &wire_forensic
        );
    }
}
