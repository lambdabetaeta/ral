use super::*;
use ral_core::data::datum::Datum;
use ral_core::fact::{
    ConnectionClosed, ConnectionFault, ConnectionFaultUnexpectedFrame, FrameKind, HelperFailure,
    ProtocolFailure, ProtocolFailureVersionMismatch, ProtocolPeer, ProtocolPhase, Reason,
};
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
            ChildStatus::Failed(ChildFailure::Cause(HelperFailure::Cancelled)),
            json!({ "failed": { "cause": HelperFailure::Cancelled.encode() } }),
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
        ChildStatus::Failed(ChildFailure::Cause(HelperFailure::Cancelled)).settlement(),
        Settlement::Failed {
            reason: ChildFailure::Cause(HelperFailure::Cancelled),
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
                cause: Severed::Refused(ral_core::protocol::WireError::of(
                    &ral_core::error::Error::new(Reason::Protocol(Box::new(
                        ProtocolFailure::VersionMismatch(ProtocolFailureVersionMismatch {
                            front: 43,
                            engine: 44,
                        }),
                    )))
                    .with_hint("upgrade"),
                )),
            }),
            json!({ "engine_severed": { "cause": { "Refused": {
                "reason": Reason::Protocol(Box::new(ProtocolFailure::VersionMismatch(ProtocolFailureVersionMismatch { front: 43, engine: 44 }))).encode(),
                "context": null,
                "hints": ["upgrade"],
            } } } }),
        ),
        (
            Record::Forensic(Forensic::EngineSevered {
                cause: Severed::Closed(ConnectionClosed::Eof(ProtocolPeer::Engine)),
            }),
            json!({ "engine_severed": { "cause": { "Closed": ConnectionClosed::Eof(ProtocolPeer::Engine).encode() } } }),
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
                cause: Severed::Faulted(ConnectionFault::UnexpectedFrame(
                    ConnectionFaultUnexpectedFrame {
                        receiver: ProtocolPeer::Frontend,
                        phase: ProtocolPhase::Attached,
                        actual: FrameKind::Attach,
                    },
                )),
            }),
            json!({ "engine_severed": { "cause": { "Faulted": ConnectionFault::UnexpectedFrame(ConnectionFaultUnexpectedFrame { receiver: ProtocolPeer::Frontend, phase: ProtocolPhase::Attached, actual: FrameKind::Attach }).encode() } } }),
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

#[test]
fn child_failures_keep_original_refusals_separate_from_portable_causes_and_panics() {
    let error = ral_core::error::Error::new(Reason::Protocol(Box::new(
        ProtocolFailure::VersionMismatch(ProtocolFailureVersionMismatch {
            front: 43,
            engine: 44,
        }),
    )))
    .with_context("helper attachment");
    assert_eq!(WireError::of(&error).hints, Vec::<String>::new());
    let error = error.with_hint("first").with_hint("second");
    let wire = WireError::of(&error);
    let failure = ChildFailure::severed(&Severed::Refused(wire.clone()));
    assert_eq!(failure, ChildFailure::Refused(wire));
    let ChildFailure::Refused(refusal) = &failure else {
        panic!("original refusal")
    };
    let restored = refusal.to_error();
    assert_eq!(restored.reason, error.reason);
    assert_eq!(restored.context(), error.context());
    assert_eq!(restored.hints(), error.hints());
    for failure in [
        failure,
        ChildFailure::Cause(HelperFailure::Panicked),
        ChildFailure::Panicked("host panic evidence".into()),
        ChildFailure::Cause(HelperFailure::NoReply),
    ] {
        let expected = match &failure {
            ChildFailure::Cause(cause) => json!({ "cause": cause.clone().encode() }),
            ChildFailure::Refused(wire) => json!({ "refused": {
                "reason": wire.reason.clone().encode(),
                "context": wire.context,
                "hints": wire.hints,
            } }),
            ChildFailure::Panicked(evidence) => json!({ "panicked": evidence }),
        };
        let status = ChildStatus::Failed(failure.clone());
        assert_eq!(status.settlement(), Settlement::Failed { reason: failure });
        let line = commit_line(&[Record::Control(Control::Status {
            status: status.clone(),
        })]);
        let stored: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            stored["commit"]["records"][0]["Control"]["status"]["status"],
            json!({ "failed": expected })
        );
        let (_, records) = parse_line(&line).expect("stored child status must unseal");
        let [Record::Control(Control::Status { status: back })] = records.as_slice() else {
            panic!("stored child status must remain a status");
        };
        assert_eq!(back, &status);
        if let ChildStatus::Failed(ChildFailure::Refused(wire)) = back {
            let restored = wire.to_error();
            assert_eq!(restored.reason, error.reason);
            assert_eq!(restored.context(), error.context());
            assert_eq!(restored.hints(), error.hints());
        }
        let bytes = ral_core::data::json::to_vec(&status).unwrap();
        assert_eq!(
            ral_core::data::json::from_slice::<ChildStatus>(&bytes).unwrap(),
            status
        );
    }
    let failure = ChildFailure::severed(&Severed::Closed(ConnectionClosed::TransportDropped));
    assert!(matches!(
        failure,
        ChildFailure::Cause(HelperFailure::Connection(
            ral_core::fact::ConnectionFailure::Closed(ConnectionClosed::TransportDropped)
        ))
    ));
}
