use super::*;
use ral_core::data::datum::Datum;
use ral_core::fact::{
    ConnectionClosed, HelperFailure, ProtocolFailure, ProtocolFailureVersionMismatch, Reason,
};
use serde_json::json;

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
