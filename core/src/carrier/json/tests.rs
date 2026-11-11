use super::*;
use crate::carrier::testkit::{assert_normalized, host_error, reason_families};
use crate::data::datum::tag;
use crate::data::json::{self, RawFOValue};
use crate::fact::*;
use crate::protocol::WireError;
use std::time::Duration;

fn frame_encoding() -> FrameEncodeFailure {
    FrameEncodeFailure::TooLarge(FrameEncodeFailureTooLarge {
        actual: u64::MAX.into(),
        limit: 256u64.into(),
    })
}

fn severances() -> Vec<Severed> {
    let mut values: Vec<_> = reason_families()
        .into_iter()
        .map(|reason| Severed::Refused(WireError::of(&host_error(reason))))
        .collect();
    for cause in [
        ConnectionClosed::Eof(ProtocolPeer::Engine),
        ConnectionClosed::Eof(ProtocolPeer::Frontend),
        ConnectionClosed::ReaderThreadEnded,
        ConnectionClosed::TransportDropped,
        ConnectionClosed::Io(ConnectionClosedIo {
            operation: ConnectionOperation::Write,
            cause: crate::io::failure(&std::io::Error::from_raw_os_error(5)),
        }),
        ConnectionClosed::Frame(FrameFailure::Partial(FrameFailurePartial {
            part: FramePart::Body,
            got: 2u64.into(),
            expected: u64::MAX.into(),
        })),
    ] {
        values.push(Severed::Closed(cause));
    }
    for cause in [
        ConnectionFault::UnexpectedFrame(ConnectionFaultUnexpectedFrame {
            receiver: ProtocolPeer::Frontend,
            phase: ProtocolPhase::AwaitAttach,
            actual: FrameKind::Attach,
        }),
        ConnectionFault::ReadingForDispatch,
        ConnectionFault::ProbeAnswer(ConnectionFaultProbeAnswer {
            probe: ProbeSubject::PathEntries(crate::text::quote_native(std::ffi::OsStr::new(
                "/safe",
            ))),
            cause: DecodeFailure::new(DecodeCause::MissingField("name".into()))
                .index(u64::MAX.into()),
        }),
        ConnectionFault::ProbeRejected(ConnectionFaultProbeRejected {
            probe: ProbeSubject::Cwd,
            cause: ProbeFailure::Busy,
        }),
        ConnectionFault::Unencodable(ConnectionFaultUnencodable {
            operation: ProtocolOperation::Answer,
            cause: frame_encoding(),
        }),
        ConnectionFault::WorkersSurvived(u64::MAX.into()),
    ] {
        values.push(Severed::Faulted(cause));
    }
    values.push(Severed::Silent(Duration::new(u64::MAX, 999_999_999)));
    values
}

#[test]
fn every_severance_keeps_canonical_causes_and_successful_serde_shapes() {
    for value in severances() {
        for (format, bytes) in [
            (JsonFormat::Compact, json::to_vec(&value).unwrap()),
            (JsonFormat::Pretty, json::to_vec_pretty(&value).unwrap()),
        ] {
            assert_eq!(bytes, json::serialize_structural(&value, format));
            let back: Severed = json::from_slice(&bytes).unwrap();
            assert_eq!(back, value);
            assert_eq!(back.code(), value.code());
            assert_eq!(back.to_string(), value.to_string());
            assert_eq!(back.connection_failure(), value.connection_failure());
            if let Severed::Refused(wire) = back {
                let original = host_error(wire.reason.clone());
                assert_normalized(&original, &wire.to_error());
                assert!(value.connection_failure().is_none());
            }
        }
    }
}

#[test]
fn raw_severed_is_the_public_nested_record_adapter() {
    #[derive(Debug, Serialize, Deserialize)]
    struct RawRecord {
        cause: RawSevered,
    }
    for value in severances() {
        let bytes = json::serialize_structural(
            &RawRecord {
                cause: RawSevered::from_value(&value),
            },
            JsonFormat::Compact,
        );
        let doc = json::JsonDocument::parse(&bytes).unwrap();
        let raw: RawRecord = json::decode_structural(doc.root()).unwrap();
        assert_eq!(raw.cause.validate().unwrap(), value);
    }
}

#[test]
fn malformed_severance_facts_keep_logical_datum_paths() {
    let cases = [
        (
            RawSevered(RawSeveredKind::Closed(RawDatum::new(
                RawFOValue::from_value(tag("eof", Some(tag("not-a-peer", None)))),
            ))),
            vec![
                DataStep::Payload("Closed".into()),
                DataStep::Payload("eof".into()),
            ],
        ),
        (
            RawSevered(RawSeveredKind::Faulted(RawDatum::new(
                RawFOValue::from_value(tag(
                    "workers-survived",
                    Some(crate::data::FOValue::Map {
                        entries: vec![
                            ("high".into(), crate::data::FOValue::Int { value: -1 }),
                            ("low".into(), crate::data::FOValue::Int { value: 0 }),
                        ],
                    }),
                )),
            ))),
            vec![
                DataStep::Payload("Faulted".into()),
                DataStep::Payload("workers-survived".into()),
                DataStep::Field("high".into()),
            ],
        ),
    ];
    for (raw, path) in cases {
        let bytes = json::serialize_structural(&raw, JsonFormat::Compact);
        let JsonInputFailure::Decode(error) = json::from_slice::<Severed>(&bytes).unwrap_err()
        else {
            panic!("valid syntax")
        };
        assert_eq!(error.path, path);
    }
}

#[test]
fn refusal_reason_and_silence_failures_keep_enclosing_payload_paths() {
    let reason = RawDatum::<Reason>::new(RawFOValue::from_value(tag("not-a-reason", None)));
    let bad_reason = json::serialize_structural(&reason, JsonFormat::Compact);
    let input = format!(
        r#"{{"Refused":{{"reason":{},"hints":[]}}}}"#,
        String::from_utf8(bad_reason).unwrap()
    );
    let JsonInputFailure::Decode(error) =
        json::from_slice::<Severed>(input.as_bytes()).unwrap_err()
    else {
        panic!("valid syntax")
    };
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Refused".into()),
            DataStep::Field("reason".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::UnknownTag(_)));

    let JsonInputFailure::Decode(error) = json::from_slice::<Severed>(
        br#"{"Silent":{"secs":18446744073709551615,"nanos":1000000000}}"#,
    )
    .unwrap_err() else {
        panic!("valid syntax")
    };
    assert_eq!(error.path, [DataStep::Payload("Silent".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::DurationOverflow(DurationOverflow {
            seconds: u64::MAX.into(),
            nanos: 1_000_000_000,
        })
    );
    let back: Severed = json::from_slice(br#"{"Silent":{"secs":2,"nanos":1000000001}}"#).unwrap();
    assert_eq!(back, Severed::Silent(Duration::new(3, 1)));
}

#[test]
fn sentence_only_severances_and_message_only_refusals_are_not_accepted() {
    for bytes in [
        br#"{"Closed":"eof"}"#.as_slice(),
        br#"{"Faulted":"junk"}"#,
        br#"{"Refused":{"message":"old","hints":[]}}"#,
    ] {
        assert!(matches!(
            json::from_slice::<Severed>(bytes),
            Err(JsonInputFailure::Decode(_))
        ));
    }
}

#[test]
fn raw_severed_borrowed_and_consuming_validation_are_equal_and_repeatable() {
    for value in severances() {
        let raw = RawSevered::from_value(&value);
        assert_eq!(raw.validate_ref(), Ok(value.clone()));
        assert_eq!(raw.validate_ref(), Ok(value.clone()));
        assert_eq!(raw.validate(), Ok(value));
    }
}

#[test]
fn raw_severed_borrowed_and_consuming_failures_keep_exact_paths() {
    for (input, path) in [
        (
            r#"{"Refused":{"reason":{"variant":{"label":"unknown"}},"context":"report","hints":["one","two"]}}"#,
            vec![
                DataStep::Payload("Refused".into()),
                DataStep::Field("reason".into()),
            ],
        ),
        (
            r#"{"Closed":"unit"}"#,
            vec![DataStep::Payload("Closed".into())],
        ),
        (
            r#"{"Faulted":"unit"}"#,
            vec![DataStep::Payload("Faulted".into())],
        ),
        (
            r#"{"Silent":{"secs":18446744073709551615,"nanos":1000000000}}"#,
            vec![DataStep::Payload("Silent".into())],
        ),
    ] {
        let doc = json::JsonDocument::parse(input.as_bytes()).unwrap();
        let raw: RawSevered = json::decode_structural(doc.root()).unwrap();
        let borrowed = raw.validate_ref().unwrap_err();
        assert_eq!(borrowed.path, path);
        assert_eq!(raw.validate_ref(), Err(borrowed.clone()));
        assert_eq!(raw.validate(), Err(borrowed));
    }
}

#[test]
fn raw_severed_borrowed_validation_drains_deep_malformed_reason() {
    let deep: RawFOValue = (0..20_000).fold(RawFOValue::Unit, |value, _| RawFOValue::List {
        items: vec![value],
    });
    let bytes = json::serialize_structural(&deep, JsonFormat::Compact);
    let input = format!(
        r#"{{"Refused":{{"reason":{},"hints":[]}}}}"#,
        String::from_utf8(bytes).unwrap()
    );
    let doc = json::JsonDocument::parse(input.as_bytes()).unwrap();
    let raw: RawSevered = json::decode_structural(doc.root()).unwrap();
    let error = raw.validate_ref().unwrap_err();
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Refused".into()),
            DataStep::Field("reason".into())
        ]
    );
    assert!(matches!(error.cause.as_ref(), DecodeCause::Shape(_)));
    assert_eq!(raw.validate_ref(), Err(error.clone()));
    assert_eq!(raw.validate(), Err(error));
}

#[test]
fn raw_severed_valid_leaf_precedes_later_record_failure() {
    #[derive(Deserialize)]
    struct RawRecord {
        severed: RawSevered,
        later: RawDatum<Count>,
    }
    let doc = json::JsonDocument::parse(
        br#"{"severed":{"Silent":{"secs":2,"nanos":1000000001}},"later":"unit"}"#,
    )
    .unwrap();
    let raw: RawRecord = json::decode_structural(doc.root()).unwrap();
    let validate = || -> Result<_, DecodeFailure> {
        let severed = raw.severed.validate_ref().map_err(|e| e.field("severed"))?;
        let later = raw.later.validate().map_err(|e| e.field("later"))?;
        Ok((severed, later))
    };
    let error = validate().unwrap_err();
    assert_eq!(error.path, [DataStep::Field("later".into())]);
    assert!(matches!(error.cause.as_ref(), DecodeCause::Shape(_)));
    assert_eq!(validate(), Err(error));
    assert_eq!(
        raw.severed.validate_ref(),
        Ok(Severed::Silent(Duration::new(3, 1)))
    );
}
