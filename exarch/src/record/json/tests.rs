//! Source fixtures for the record representation boundary; no log I/O.
use super::chat::{RawCustomPart, RawForeignValue, RawToolCall};
use super::*;
use crate::card::{Field, FieldVal, Mark, Measure, Readout, Seg};
use ral_core::data::datum::tag;
use ral_core::data::json::{self, JsonDocument};
use ral_core::fact::{
    ConnectionClosed, ConnectionFault, CredentialCause, CredentialService, DataStep, ErrorFact,
    JsonInputFailure, ProtocolPeer, ProviderStop, RecordingFailureCellPoisoned, ServiceCause,
    ServiceOperation, StateFailure, StreamTimeoutKind,
};
use std::time::Duration;

fn failure<T: JsonDecode>(text: &str) -> DecodeFailure {
    match json::from_slice::<T>(text.as_bytes()) {
        Err(JsonInputFailure::Decode(error)) => error,
        _ => panic!("expected a typed decode refusal"),
    }
}

fn round_trip<T: JsonDecode + JsonEncode + Serialize>(value: &T) {
    for (format, bytes) in [
        (JsonFormat::Compact, json::to_vec(value).unwrap()),
        (JsonFormat::Pretty, json::to_vec_pretty(value).unwrap()),
    ] {
        assert_eq!(bytes, serialize_structural(value, format));
        let back: T = json::from_slice(&bytes).unwrap();
        assert_eq!(json::to_vec(&back).unwrap(), json::to_vec(value).unwrap());
    }
}

fn wire() -> WireError {
    WireError {
        reason: Reason::State(Box::new(StateFailure::NoDesk)),
        context: Some("host context".into()),
        hints: vec!["first".into(), "second".into()],
    }
}

fn service() -> ServiceFailure {
    ServiceFailure {
        service: "fixture-service".into(),
        operation: ServiceOperation::Chat,
        context: vec![],
        cause: Box::new(ServiceCause::StreamTimeout(StreamTimeoutKind::Read)),
    }
}

fn recording() -> RecordingFailure {
    RecordingFailure::CellPoisoned(RecordingFailureCellPoisoned {
        agent: u64::MAX.into(),
    })
}

fn selected() -> Selected {
    Selected::for_test("fixture-model")
}

#[test]
fn record_json_child_failures_keep_canonical_helper_and_original_refusal() {
    for child in [
        ChildFailure::Cause(HelperFailure::ProviderStopped(ProviderStop::ContentFilter(
            "blocked".into(),
        ))),
        ChildFailure::Cause(HelperFailure::Panicked),
        ChildFailure::Refused(wire()),
        ChildFailure::Panicked("lifecycle evidence".into()),
    ] {
        round_trip(&child);
        round_trip(&ChildStatus::Failed(child.clone()));
        round_trip(&Settlement::Failed {
            reason: child.clone(),
        });
        let back: ChildFailure = json::from_slice(&json::to_vec(&child).unwrap()).unwrap();
        assert_eq!(back, child);
    }
    let back: ChildFailure =
        json::from_slice(&json::to_vec(&ChildFailure::Refused(wire())).unwrap()).unwrap();
    assert!(matches!(back, ChildFailure::Refused(found) if found == wire()));
    assert!(matches!(
        *failure::<ChildStatus>(r#"{"failed":"old sentence"}"#).cause,
        DecodeCause::UnknownTag(_)
    ));
}

#[test]
fn record_json_all_provider_carrier_arms_keep_existing_spelling_and_datum_fields() {
    let at = "2026-11-10T03:00:00Z".parse().unwrap();
    for error in [
        ProviderError::Unavailable(recording()),
        ProviderError::Cancelled(CancelSite::Backoff),
        ProviderError::Transient {
            cause: RetryableProviderCause::Service(service()),
            attempts: u32::MAX,
        },
        ProviderError::Refused(Refusal::held(
            Limit::Allowance,
            Some(at),
            at,
            "service",
            "model",
        )),
        ProviderError::Api {
            model: "m".into(),
            cause: service(),
        },
        ProviderError::Overflow {
            model: "m".into(),
            provider: Some("upstream".into()),
            sent: Some(u64::MAX),
            limit: Some(0),
            listed: None,
            cause: Box::new(service()),
        },
        ProviderError::Truncated {
            cause: Box::new(CutShort::OutputCap {
                stop_reason: "length".into(),
            }),
        },
        ProviderError::Truncated {
            cause: Box::new(CutShort::Stalled(ProviderError::Terminal(service()))),
        },
        ProviderError::Terminal(service()),
        ProviderError::Credential(CredentialFailure {
            account: "a".into(),
            service: CredentialService::Codex,
            cause: CredentialCause::Removed,
        }),
    ] {
        round_trip(&error);
        round_trip(&Forensic::ProviderError {
            error: error.clone(),
        });
        round_trip(&Forensic::Retried { error });
    }
}

#[test]
fn record_json_timestamp_parser_is_the_named_foreign_boundary() {
    let cause = RawDatum::from_datum(ProviderRefusalCause::Service(service()));
    let cause = String::from_utf8(serialize_structural(&cause, JsonFormat::Compact)).unwrap();
    let text = format!(
        r#"{{"limit":"rate","resets_at":"bad-reset","received":"bad-received","cause":{cause}}}"#
    );
    let failure = failure::<Refusal>(&text);
    assert_eq!(failure.path, vec![DataStep::Field("resets_at".into())]);
    assert!(
        matches!(*failure.cause, DecodeCause::Timestamp(c) if c.text == "bad-reset" && !c.detail.is_empty())
    );
}

#[test]
fn record_json_via_precedence_and_present_value_paths_are_typed() {
    for (tags, expected) in [
        ("[]", ViaFailure::Empty),
        (r#"["a","a",""]"#, ViaFailure::EmptyTag),
        (
            r#"["a","b","a"]"#,
            ViaFailure::DuplicateTag(ViaFailureDuplicateTag { tag: "a".into() }),
        ),
    ] {
        let text = format!(
            r#"{{"model":"m","label":"l","service":"s","account":"a","via":{tags},"tuning":{{}}}}"#
        );
        let failure = failure::<Selected>(&text);
        assert_eq!(failure.path, vec![DataStep::Field("via".into())]);
        assert_eq!(*failure.cause, DecodeCause::Via(expected));
    }
}

#[test]
fn record_json_nonempty_body_and_post_lists_retain_length_causes() {
    let doc = JsonDocument::parse(br#"{"at_unix_ms":0,"records":[]}"#).unwrap();
    let body_failure = RawBody::read(doc.root()).unwrap().validate().unwrap_err();
    assert_eq!(body_failure.path, vec![DataStep::Field("records".into())]);
    assert_eq!(
        *body_failure.cause,
        DecodeCause::Length(DecodeLength {
            expected: CountRule::AtLeast(1u64.into()),
            got: 0u64.into()
        })
    );
    for text in [r#"{"taken":{"posts":[]}}"#, r#"{"withdrawn":{"posts":[]}}"#] {
        assert!(matches!(
            *failure::<Control>(text).cause,
            DecodeCause::Length(_)
        ));
    }
}

#[test]
fn record_json_body_is_object_only_strict_and_preserves_original_bytes_at_caller() {
    for text in [
        r"[0,[]]",
        r#"{"at_unix_ms":0,"records":[],"other":0}"#,
        r#"{"at_unix_ms":0,"at_unix_ms":1,"records":[]}"#,
    ] {
        let doc = JsonDocument::parse(text.as_bytes()).unwrap();
        assert!(RawBody::read(doc.root()).is_err());
    }
    let original = br#"{ "records" : [ {"Forensic":"session_ended"} ], "at_unix_ms": 0 }"#;
    let doc = JsonDocument::parse(original).unwrap();
    let raw = RawBody::read(doc.root()).unwrap();
    assert_eq!(doc.root().source(), original);
    let records = raw.validate().unwrap();
    let encoded = RawBody::from_records(0, &records)
        .unwrap()
        .encode(JsonFormat::Compact);
    assert_ne!(encoded, original);
    assert_eq!(Sum::of(doc.root().source()), Sum::of(original));
}

#[test]
fn record_json_span_and_nested_locus_seal_keep_domain_operands() {
    let error = failure::<Span>(r#"{"from":18446744073709551615,"to":1}"#);
    assert!(
        matches!(*error.cause, DecodeCause::TurnSpan(c) if c.from == u64::MAX.into() && c.to == 1u64.into())
    );
    let error = failure::<Pointer>(
        r#"{"source":1,"loci":[{"seq":{"commit":1,"index":0},"bytes":{"start":0,"end":1},"sum":"ABCDEF0000000000"}]}"#,
    );
    assert_eq!(
        error.path,
        vec![
            DataStep::Field("loci".into()),
            DataStep::Index(0u64.into()),
            DataStep::Field("sum".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::SealSpelling(_)));
}

#[test]
fn record_json_context_links_round_trip_all_affected_ancestors() {
    let locus = Locus {
        seq: Seq::new(u64::MAX, 2),
        bytes: 4..12,
        sum: Sum::of(b"fixture"),
    };
    let pointer = Pointer {
        source: AgentId::new(u64::MAX),
        loci: vec![locus],
    };
    for location in [
        Location::Resident,
        Location::Evicted(Span::new(1, 9).unwrap()),
        Location::Withdrawn,
    ] {
        let row = TurnRow {
            id: u64::MAX,
            role: Role::Assistant,
            kind: TurnKind::Inherited,
            label: "turn".into(),
            bytes: usize::MAX,
            location,
        };
        round_trip(&row);
        round_trip(&Linked {
            row,
            at: pointer.clone(),
        });
        round_trip(&SpanRow {
            span: Span::single(u64::MAX),
            role: Role::User,
            kind: ItemKind::Eviction,
            label: "span".into(),
            bytes: usize::MAX,
            location,
        });
    }
    round_trip(&Eviction {
        span: Span::new(1, u64::MAX).unwrap(),
        note: Some("note".into()),
        by: EditAuthority::Model,
    });
}

#[test]
fn record_json_settlement_internal_tags_keep_order_duplicates_units_and_sequences() {
    for text in [
        r#"{"reason":{"panicked":"evidence"},"outcome":"failed"}"#,
        r#"["failed",{"panicked":"evidence"}]"#,
    ] {
        let result: Settlement = json::from_slice(text.as_bytes()).unwrap();
        assert_eq!(
            result,
            Settlement::Failed {
                reason: ChildFailure::Panicked("evidence".into())
            }
        );
    }
    let _: Settlement = json::from_slice(br#"{"outcome":"replied","ignored":1}"#).unwrap();
    let _: Settlement = json::from_slice(br#"["stopped"]"#).unwrap();
    assert!(
        matches!(*failure::<Settlement>(r#"{"outcome":"failed","outcome":"failed"}"#).cause, DecodeCause::DuplicateField(f) if f == "outcome")
    );
    assert!(matches!(
        *failure::<Settlement>(r#"{"outcome":"failed","reason":{"panicked":"x"},"extra":1}"#).cause,
        DecodeCause::UnknownField(_)
    ));
    assert!(matches!(
        *failure::<Settlement>(r#"["stopped",1]"#).cause,
        DecodeCause::Length(_)
    ));
}

fn card() -> Card {
    Card(vec![
        Mark::Text {
            spans: vec![crate::card::Span {
                role: Some(crate::card::Role::Strong),
                text: "title".into(),
            }],
        },
        Mark::Measure(Measure {
            label: "count".into(),
            readout: Readout {
                value: u32::MAX,
                max: Some(u32::MAX),
                unit: Some("bytes".into()),
            },
        }),
        Mark::Fields {
            rows: vec![
                Field {
                    label: "field".into(),
                    value: FieldVal::Inline(vec![]),
                },
                Field {
                    label: "readout".into(),
                    value: FieldVal::Readout(Readout {
                        value: 1,
                        max: None,
                        unit: None,
                    }),
                },
            ],
        },
        Mark::Diff {
            path: "file".into(),
            hunks: vec![Hunk {
                old: 1,
                new: 2,
                rows: vec![
                    Row::Context(vec![Seg::plain("context")]),
                    Row::Del(vec![]),
                    Row::Add(vec![Seg {
                        emph: true,
                        text: "new".into(),
                    }]),
                ],
            }],
        },
        Mark::Raw {
            bytes: vec![0, 255],
        },
    ])
}

#[test]
fn record_json_card_tags_flattened_measures_and_adjacent_rows_preserve_bytes() {
    round_trip(&card());
    round_trip(&Display::Card { card: card() });
    round_trip(&Change {
        path: "file".into(),
        outcome: WriteOutcome::Committed,
        diff: Some(Diff {
            hunks: vec![Hunk {
                old: 0,
                new: 1,
                rows: vec![Row::Add(vec![])],
            }],
            added: 1,
            removed: 0,
        }),
    });
    let _: Card =
        json::from_slice(br#"[["text",[]],["fields",[]],["diff","p",[]],["raw",[0,255]]]"#)
            .unwrap();
    let row: Row = json::from_slice(br#"{"segs":[],"tag":"add","ignored":true}"#).unwrap();
    assert!(matches!(row, Row::Add(v) if v.is_empty()));
    let _: Row = json::from_slice(br#"["del",[]]"#).unwrap();
}

#[test]
fn record_json_card_recognized_duplicates_and_missing_tag_content_are_typed() {
    for (text, field) in [
        (r#"[{"mark":"text","mark":"raw","spans":[]}]"#, "mark"),
        (
            r#"[{"mark":"measure","label":"a","value":1,"value":2}]"#,
            "value",
        ),
        (
            r#"[{"mark":"measure","label":"a","label":"b","value":1}]"#,
            "label",
        ),
    ] {
        assert!(
            matches!(*failure::<Card>(text).cause, DecodeCause::DuplicateField(found) if found == field)
        );
    }
    assert!(
        matches!(*failure::<Row>(r#"{"tag":"add","segs":[],"segs":[]}"#).cause, DecodeCause::DuplicateField(f) if f == "segs")
    );
    assert!(
        matches!(*failure::<Row>(r#"{"tag":"add"}"#).cause, DecodeCause::MissingField(f) if f == "segs")
    );
    assert!(
        matches!(*failure::<Card>(r#"[{"spans":[]}]"#).cause, DecodeCause::MissingField(f) if f == "mark")
    );
    assert!(matches!(
        *failure::<Measure>(r#"["label",1]"#).cause,
        DecodeCause::JsonShape(_)
    ));
}

#[test]
fn record_json_card_selected_payload_paths_and_byte_ranges_are_structured() {
    let error =
        failure::<Record>(r#"{"Display":{"card":{"card":[{"mark":"raw","bytes":[256]}]}}}"#);
    assert_eq!(
        error.path,
        vec![
            DataStep::Payload("Display".into()),
            DataStep::Payload("card".into()),
            DataStep::Field("card".into()),
            DataStep::Index(0u64.into()),
            DataStep::Payload("raw".into()),
            DataStep::Field("bytes".into()),
            DataStep::Index(0u64.into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::JsonNumber(c) if c.text == "256"));
}

#[test]
fn record_json_deep_data_prefixes_are_not_hydrated_before_later_refusal() {
    let deep = format!(
        "{}\"unit\"{}",
        r#"{"list":{"items":["#.repeat(4000),
        "]}}".repeat(4000)
    );
    let text = format!(
        r#"{{"at_unix_ms":0,"records":[{{"Display":{{"observation":{{"value":{deep}}}}}}},{{"Transcript":{{"evicted":{{"span":{{"from":2,"to":1}},"by":"model"}}}}}}]}}"#
    );
    let doc = JsonDocument::parse(text.as_bytes()).unwrap();
    let raw = RawBody::read(doc.root()).unwrap();
    assert!(matches!(
        *raw.validate().unwrap_err().cause,
        DecodeCause::TurnSpan(_)
    ));
    drop(raw);
    let structural = format!(
        r#"{{"at_unix_ms":0,"records":[{{"Display":{{"observation":{{"value":{deep}}}}}}},{{"Transcript":{{"system":{{"text":false}}}}}}]}}"#
    );
    let doc = JsonDocument::parse(structural.as_bytes()).unwrap();
    assert!(RawBody::read(doc.root()).is_err());
}

#[test]
fn record_json_raw_recursive_provider_drop_survives_a_later_field_refusal() {
    let prefix = r#"{"truncated":{"cause":{"stalled":"#.repeat(4000);
    let deep = format!(
        "{prefix}{{\"cancelled\":\"backoff\"}}{}",
        "}}}".repeat(4000)
    );
    let text = format!(r#"{{"Forensic":{{"retried":{{"error":{deep},"extra":0}}}}}}"#);
    assert!(matches!(
        *failure::<Record>(&text).cause,
        DecodeCause::UnknownField(_)
    ));
    let text = format!(
        r#"{{"at_unix_ms":0,"records":[{{"Forensic":{{"retried":{{"error":{deep}}}}}}},{{"Control":{{"taken":{{"posts":[]}}}}}}]}}"#
    );
    let doc = JsonDocument::parse(text.as_bytes()).unwrap();
    assert!(matches!(
        *RawBody::read(doc.root())
            .unwrap()
            .validate()
            .unwrap_err()
            .cause,
        DecodeCause::Length(_)
    ));
}

#[test]
fn record_json_foreign_json_prefix_cleanup_and_destination_replacement_are_safe() {
    let deep = format!("{}0{}", "[".repeat(4000), "]".repeat(4000));
    let text = format!(
        r#"{{"import":{{"turn":1,"message":{{"role":"User","content":[{{"Custom":{{"data":{deep},"model_iden":false}}}}]}}}}}}"#
    );
    assert!(matches!(
        *failure::<Transcript>(&text).cause,
        DecodeCause::JsonShape(_)
    ));
    let text = format!(
        r#"{{"import":{{"turn":1,"message":{{"role":"User","content":[{{"Custom":{{"data":{{"x":{deep},"x":1}}}}}}]}}}}}}"#
    );
    let back: Transcript = json::from_slice(text.as_bytes()).unwrap();
    let Transcript::Import { message, .. } = back else {
        panic!("import")
    };
    assert!(
        matches!(message.content.iter().next(), Some(ContentPart::Custom(part)) if part.data["x"] == 1)
    );
}

#[test]
fn record_json_transcript_chat_structural_contract_keeps_model_name_asymmetry() {
    let text = br#"{"import":{"turn":18446744073709551615,"message":{"role":"User","content":[{"Custom":{"model_iden":{"adapter_kind":"OpenAI","model_name":"plain"},"data":{"x":18446744073709551615}}}]}}}"#;
    let back: Transcript = json::from_slice(text).unwrap();
    let bytes = json::to_vec(&back).unwrap();
    assert_eq!(bytes, serialize_structural(&back, JsonFormat::Compact));
    assert!(
        json::from_slice::<Transcript>(&bytes).is_err(),
        "pinned ModelName writes an enum but reads a borrowed string"
    );
    let escaped = std::str::from_utf8(text)
        .unwrap()
        .replace("plain", r"pl\u0061in");
    assert!(matches!(
        *failure::<Transcript>(&escaped).cause,
        DecodeCause::JsonString(_)
    ));
}

#[test]
fn record_json_nonfinite_bits_and_base64_use_existing_pure_validators() {
    for text in [
        r#"{"replied":{"float":{"value":9218868437227405312}}}"#,
        r#"{"replied":{"bytes":{"value":"!"}}}"#,
    ] {
        let error = failure::<ChildStatus>(text);
        assert!(matches!(
            *error.cause,
            DecodeCause::FiniteBits(_) | DecodeCause::Base64(_)
        ));
        assert!(matches!(error.path.first(), Some(DataStep::Payload(tag)) if tag == "replied"));
    }
}

#[test]
fn record_json_severances_use_repeated_borrowed_validation_and_later_checks() {
    for cause in [
        Severed::Refused(wire()),
        Severed::Closed(ConnectionClosed::Eof(ProtocolPeer::Engine)),
        Severed::Silent(Duration::new(u64::MAX, 999_999_999)),
        Severed::Faulted(ConnectionFault::ReadingForDispatch),
    ] {
        round_trip(&Forensic::EngineSevered {
            cause: cause.clone(),
        });
        let raw = RawSevered::from_value(&cause);
        assert_eq!(raw.validate_ref().unwrap(), cause);
        assert_eq!(raw.validate_ref().unwrap(), cause);
        assert_eq!(raw.validate().unwrap(), cause);
    }
    let first = serialize_structural(
        &Record::Forensic(Forensic::EngineSevered {
            cause: Severed::Refused(wire()),
        }),
        JsonFormat::Compact,
    );
    let text = format!(
        r#"{{"at_unix_ms":0,"records":[{},{{"Control":{{"taken":{{"posts":[]}}}}}}]}}"#,
        std::str::from_utf8(&first).unwrap()
    );
    let doc = JsonDocument::parse(text.as_bytes()).unwrap();
    assert!(matches!(
        *RawBody::read(doc.root())
            .unwrap()
            .validate()
            .unwrap_err()
            .cause,
        DecodeCause::Length(_)
    ));
}

#[test]
fn record_json_done_carries_the_small_error_record() {
    let record = ErrorFact::new(
        wire().reason,
        "operation".into(),
        ral_core::test_helper::site(),
    )
    .record();
    let done = Display::Done {
        id: Some(u64::MAX),
        title: "work".into(),
        elapsed_ms: None,
        outcome: Done::Err(record.clone()),
    };
    round_trip(&done);
    let text = json::to_vec(&done).unwrap();
    let Display::Done {
        outcome: Done::Err(back),
        ..
    } = json::from_slice(&text).unwrap()
    else {
        panic!("error ending")
    };
    assert_eq!(back, record);
    let malformed = tag("err", Some(FOValue::Map { entries: vec![] }));
    let raw = RawDatum::<Done>::new(RawFOValue::from_value(malformed));
    assert!(raw.validate().is_err());
}

#[cfg(unix)]
#[test]
fn record_json_non_unicode_started_path_is_refused_before_any_body_encoding() {
    use std::os::unix::ffi::OsStringExt;
    let started = Started {
        session_id: AgentId::new(1),
        name: "agent".into(),
        parent: None,
        returns: false,
        selected: selected(),
        log_dir: std::ffi::OsString::from_vec(vec![b'/', 0xff]).into(),
        at_unix_ms: 0,
    };
    let record = Record::Control(Control::SessionStarted(started));
    let error = json::to_vec(&record).unwrap_err();
    assert_eq!(
        error.path,
        vec![
            DataStep::Payload("Control".into()),
            DataStep::Payload("session_started".into()),
            DataStep::Field("log_dir".into())
        ]
    );
    assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
}

#[test]
fn record_json_ordinary_products_keep_sequence_optional_unknown_and_duplicate_rules() {
    let _: Selected = json::from_slice(br#"["m",null,"l","s","a",null,{},null]"#).unwrap();
    assert_eq!(
        failure::<Selected>(r#"["m",null,"l","s","a",null,{}]"#)
            .cause
            .as_ref(),
        &DecodeCause::Length(DecodeLength {
            expected: CountRule::Exactly(8u64.into()),
            got: 7u64.into(),
        })
    );
    assert!(matches!(
        *failure::<Selected>(
            r#"{"model":"m","label":"l","service":"s","account":"a","tuning":{},"extra":0}"#
        )
        .cause,
        DecodeCause::UnknownField(_)
    ));
    assert!(
        matches!(*failure::<Transcript>(r#"{"system":{"text":"first","text":"second"}}"#).cause, DecodeCause::DuplicateField(f) if f == "text")
    );
    let locus: Locus = json::from_slice(br#"{"seq":{"commit":1,"index":0},"bytes":{"start":0,"end":1},"sum":"0123456789abcdef","ignored":true}"#).unwrap();
    assert_eq!(locus.seq(), Seq::new(1, 0));
    assert_eq!(locus.bytes(), 0..1);
    assert_eq!(locus.sum().to_string(), "0123456789abcdef");
}

#[test]
fn record_json_every_record_arm_matches_successful_compact_and_pretty_serde() {
    let eviction = Eviction {
        span: Span::single(1),
        note: None,
        by: EditAuthority::Harness,
    };
    let selected = selected();
    let transcripts = vec![
        Transcript::System {
            text: "system".into(),
        },
        Transcript::UserPrompt {
            turn: u64::MAX,
            text: "prompt".into(),
            voice: Voice::Human,
        },
        Transcript::Steering {
            text: "steer".into(),
            voice: Voice::Harness,
        },
        Transcript::Import {
            turn: 1,
            message: ChatMessage::user("import"),
        },
        Transcript::Inherited {
            turns: vec![],
            evictions: vec![eviction.clone()],
        },
        Transcript::AssistantMessage {
            turn: 2,
            message: ChatMessage::assistant("reply"),
            pending_tool_ids: vec!["call".into()],
            stop_reason: None,
        },
        Transcript::ToolResults {
            results: vec![ToolResult {
                id: "call".into(),
                content: "result".into(),
            }],
        },
        Transcript::Evicted(eviction),
        Transcript::Rewound { anchor: 1 },
        Transcript::Cleared,
    ];
    let controls = vec![
        Control::SessionStarted(Started {
            session_id: AgentId::new(1),
            name: "agent".into(),
            parent: None,
            returns: true,
            selected: selected.clone(),
            log_dir: "session".into(),
            at_unix_ms: u64::MAX,
        }),
        Control::SessionResumed {
            selected: selected.clone(),
            returns: false,
            at_unix_ms: 0,
        },
        Control::JobOpened,
        Control::JobSettled {
            job: JobId::from(1),
            stopped: true,
        },
        Control::Status {
            status: ChildStatus::Stopped,
        },
        Control::Posted(Post {
            voice: None,
            text: "cmd".into(),
            from: Some(AgentId::new(2)),
        }),
        Control::Taken {
            posts: NonEmpty::one(PostId(1)),
        },
        Control::Withdrawn {
            posts: NonEmpty::one(PostId(2)),
        },
        Control::Scheduled {
            label: "wake".into(),
            schedule: Schedule {
                prompt: "prompt".into(),
                trigger: "tomorrow".into(),
                cron: None,
                next_unix_ms: 0,
                fires: u64::MAX,
            },
        },
        Control::Unscheduled {
            label: "wake".into(),
        },
        Control::ModelChanged { selected },
        Control::WindowNarrowed {
            model: "m".into(),
            context_window: u64::MAX,
        },
    ];
    let displays = vec![
        Display::Thinking {
            text: "think".into(),
        },
        Display::Answer {
            text: "answer".into(),
        },
        Display::ToolCall {
            tool: "ral".into(),
            cmd: "cmd".into(),
            summary: None,
        },
        Display::HarnessCall {
            verb: "read".into(),
            subject: None,
            payload: "payload".into(),
            failed: true,
        },
        Display::Verdict {
            call: BlockId::new(Seq::new(1, 0)),
            verdict: Verdict {
                lines: u32::MAX,
                failed: false,
            },
        },
        Display::SubagentDone {
            name: "child".into(),
            outcome: Settlement::Stopped,
            elapsed_ms: u64::MAX,
        },
        Display::Observation {
            value: FOValue::Float {
                value: ral_core::data::Finite::new(1.5).unwrap(),
            },
        },
        Display::Change {
            change: Change {
                path: "file".into(),
                outcome: WriteOutcome::Failed,
                diff: None,
            },
        },
        Display::Card { card: card() },
        Display::Done {
            id: None,
            title: "done".into(),
            elapsed_ms: Some(1),
            outcome: Done::Ok(()),
        },
        Display::Context { rows: vec![] },
    ];
    let forensics = vec![
        Forensic::SessionEnded,
        Forensic::SessionClosed,
        Forensic::EngineSevered {
            cause: Severed::Refused(wire()),
        },
        Forensic::TeardownFailed {
            failure: "lifecycle evidence".into(),
        },
        Forensic::TurnStarted {
            turn: u64::MAX,
            tuning: Tuning::default(),
            via: Some(Via::one("upstream".into())),
        },
        Forensic::UsageDelta {
            usage: Usage::default(),
            generation: Some("generation".into()),
            provider: None,
        },
        Forensic::Cancelled,
        Forensic::Error {
            text: "diagnostic".into(),
        },
        Forensic::Nudge {
            cause: "nudge".into(),
            spent: Some(Spent {
                used: u32::MAX,
                max: u32::MAX,
            }),
        },
        Forensic::ProviderError {
            error: ProviderError::Terminal(service()),
        },
        Forensic::Retried {
            error: ProviderError::Cancelled(CancelSite::BeforeRequest),
        },
        Forensic::SystemNote {
            text: "note".into(),
        },
        Forensic::HarnessResult {
            text: "result".into(),
        },
        Forensic::Pin { key: "pin".into() },
        Forensic::Unpin { key: "pin".into() },
        Forensic::Reap {
            title: "worker".into(),
            cause: "retention".into(),
        },
        Forensic::Prune {
            names: vec!["binding".into()],
            idle_calls: vec![u64::MAX],
        },
    ];
    for record in transcripts
        .into_iter()
        .map(Record::Transcript)
        .chain(controls.into_iter().map(Record::Control))
        .chain(displays.into_iter().map(Record::Display))
        .chain(forensics.into_iter().map(Record::Forensic))
    {
        round_trip(&record);
    }
}

#[test]
fn record_json_chat_all_content_arms_are_audited_structural_reuse() {
    let message = ChatMessage::user(genai::chat::MessageContent::from_parts(vec![
        ContentPart::Text("text".into()),
        ContentPart::Binary(Binary::from_base64(
            "image/png",
            "not-validated-as-base64",
            None,
        )),
        ContentPart::Binary(Binary::from_url(
            "image/png",
            "not-validated-as-url",
            Some("name".into()),
        )),
        ContentPart::ToolCall(ToolCall {
            call_id: "id".into(),
            fn_name: "fn".into(),
            fn_arguments: serde_json::json!({"count":u64::MAX}),
            thought_signatures: Some(vec!["signature".into()]),
        }),
        ContentPart::ToolResponse(ToolResponse::new("id", "result")),
        ContentPart::ThoughtSignature("thought".into()),
        ContentPart::ReasoningContent("reasoning".into()),
        ContentPart::Custom(CustomPart {
            model_iden: None,
            data: serde_json::json!({"x":[true,null,"text"]}),
        }),
    ]))
    .with_options(MessageOptions::default());
    round_trip(&Transcript::Import { turn: 1, message });
}

#[test]
fn record_json_deep_malformed_severance_preserves_borrowed_consuming_failures() {
    let deep = format!(
        "{}\"unit\"{}",
        r#"{"list":{"items":["#.repeat(4000),
        "]}}".repeat(4000)
    );
    let text = format!(r#"{{"Refused":{{"reason":{deep},"context":null,"hints":[]}}}}"#);
    let doc = JsonDocument::parse(text.as_bytes()).unwrap();
    let raw: RawSevered = decode_structural(doc.root()).unwrap();
    let first = raw.validate_ref().unwrap_err();
    assert_eq!(first, raw.validate_ref().unwrap_err());
    assert_eq!(first, raw.validate().unwrap_err());
    assert!(
        matches!(first.path.as_slice(), [DataStep::Payload(tag), DataStep::Field(field)] if tag == "Refused" && field == "reason")
    );
}

#[test]
fn record_json_internal_numeric_buffering_keeps_streaming_tag_precedence() {
    for text in [
        r#"{"ignored":1e400,"mark":"text","spans":[]}"#,
        r#"{"mark":"text","ignored":1e400,"spans":[]}"#,
        r#"{"ignored":1e400,"mark":"unknown"}"#,
        r#"{"mark":"text","ignored":1e400,"mark":"raw"}"#,
        r#"{"ignored":1e400}"#,
        r#"["text",[],1e400]"#,
    ] {
        assert!(
            matches!(*failure::<Mark>(text).cause, DecodeCause::JsonNumber(c) if c.text == "1e400")
        );
    }
    assert!(matches!(
        *failure::<Mark>(r#"{"mark":"unknown","ignored":1e400}"#).cause,
        DecodeCause::UnknownTag(_)
    ));
    assert!(
        matches!(*failure::<Mark>(r#"{"mark":"text","mark":1e400,"ignored":1e400}"#).cause, DecodeCause::DuplicateField(f) if f == "mark")
    );
    assert!(
        matches!(*failure::<Settlement>(r#"{"outcome":"replied","ignored":{"nested":[1e400]}}"#).cause, DecodeCause::JsonNumber(c) if c.text == "1e400")
    );
    let _: Mark = json::from_slice(br#"{"mark":"text","spans":[],"ignored":1e300}"#).unwrap();
    let _: Settlement =
        json::from_slice(br#"{"outcome":"replied","ignored":18446744073709551615}"#).unwrap();
}

#[test]
fn record_json_adjacent_numeric_buffering_depends_on_tag_content_order_only() {
    let tag_first = r#"{"tag":"add","segs":[{"emph":false,"text":"x","ignored":1e400}]}"#;
    let content_first = r#"{"segs":[{"emph":false,"text":"x","ignored":1e400}],"tag":"add"}"#;
    let _: Row = json::from_slice(tag_first.as_bytes()).unwrap();
    let error = failure::<Row>(content_first);
    assert_eq!(
        error.path,
        vec![
            DataStep::Field("segs".into()),
            DataStep::Index(0u64.into()),
            DataStep::Field("ignored".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::JsonNumber(c) if c.text == "1e400"));
    for row in [tag_first, content_first] {
        let mark =
            format!(r#"{{"mark":"diff","path":"p","hunks":[{{"old":1,"new":1,"rows":[{row}]}}]}}"#);
        assert!(
            matches!(*failure::<Mark>(&mark).cause, DecodeCause::JsonNumber(c) if c.text == "1e400")
        );
    }
    let _: Row = json::from_slice(br#"{"ignored":1e400,"tag":"add","segs":[]}"#).unwrap();
}

#[test]
fn record_json_flattened_measure_preflights_buffered_fields_not_direct_label() {
    assert!(
        matches!(*failure::<Measure>(r#"{"label":"l","value":1,"ignored":1e400}"#).cause, DecodeCause::JsonNumber(c) if c.text == "1e400")
    );
    assert!(matches!(
        *failure::<Measure>(r#"{"label":false,"ignored":1e400}"#).cause,
        DecodeCause::JsonShape(_)
    ));
    assert!(matches!(
        *failure::<Measure>(r#"{"ignored":1e400,"label":false}"#).cause,
        DecodeCause::JsonNumber(_)
    ));
    let _: Measure = json::from_slice(br#"{"label":"l","value":1,"ignored":1e300}"#).unwrap();
}

#[test]
fn record_json_foreign_objects_keep_insertion_order_and_replace_in_original_slot() {
    for (input, expected) in [
        (r#"{"z":1,"a":2}"#, r#"{"z":1,"a":2}"#),
        (r#"{"z":1,"a":2,"z":3}"#, r#"{"z":3,"a":2}"#),
    ] {
        let text = format!(r#"{{"call_id":"c","fn_name":"f","fn_arguments":{input}}}"#);
        let document = JsonDocument::parse(text.as_bytes()).unwrap();
        let raw = RawToolCall::read(document.root()).unwrap();
        raw.check().unwrap();
        let tool = raw.hydrate();
        let raw = RawToolCall::from_value(&tool).unwrap();
        assert_eq!(tool.fn_arguments.to_string(), expected);
        assert_eq!(
            serialize_structural(&raw, JsonFormat::Compact),
            format!(r#"{{"call_id":"c","fn_name":"f","fn_arguments":{expected}}}"#).as_bytes()
        );

        let text = format!(r#"{{"data":{input}}}"#);
        let document = JsonDocument::parse(text.as_bytes()).unwrap();
        let raw = RawCustomPart::read(document.root()).unwrap();
        raw.check().unwrap();
        let custom = raw.hydrate();
        let raw = RawCustomPart::from_value(&custom).unwrap();
        assert_eq!(
            serialize_structural(&raw, JsonFormat::Compact),
            format!(r#"{{"model_iden":null,"data":{expected}}}"#).as_bytes()
        );
    }
}

#[test]
fn record_json_optional_object_fields_are_required_positional_slots() {
    for text in [
        r#"["m",null,"l","s","a",null,{}]"#,
        r#"["m",null,"l","s","a"]"#,
    ] {
        assert!(serde_json::from_str::<Selected>(text).is_err());
        assert!(matches!(
            failure::<Selected>(text).cause.as_ref(),
            DecodeCause::Length(_)
        ));
    }
    let full = br#"["m",null,"l","s","a",null,{},null]"#;
    let _: Selected = json::from_slice(full).unwrap();
    let _: Selected = serde_json::from_slice(full).unwrap();
    let _: Selected =
        json::from_slice(br#"{"model":"m","label":"l","service":"s","account":"a","tuning":{}}"#)
            .unwrap();

    let document = JsonDocument::parse(br#"["User",[]]"#).unwrap();
    let error = RawChatMessage::read(document.root()).unwrap_err();
    assert_eq!(
        error.cause.as_ref(),
        &DecodeCause::Length(DecodeLength {
            expected: CountRule::Exactly(3u64.into()),
            got: 2u64.into(),
        })
    );
    let document = JsonDocument::parse(br#"["User",[],null]"#).unwrap();
    assert!(RawChatMessage::read(document.root()).is_ok());
    let document = JsonDocument::parse(br#"["c","f",{}]"#).unwrap();
    assert!(matches!(
        RawToolCall::read(document.root())
            .unwrap_err()
            .cause
            .as_ref(),
        DecodeCause::Length(_)
    ));
    let document = JsonDocument::parse(br#"["c","f",{},null]"#).unwrap();
    assert!(RawToolCall::read(document.root()).is_ok());
}

#[test]
fn record_json_foreign_raw_value_sentinel_is_special_only_in_first_member() {
    for (input, expected) in [
        (r#"{"$serde_json::private::RawValue":"1"}"#, "1"),
        (
            r#"{"$serde_json::private::RawValue":"{\"z\":1,\"a\":2,\"z\":3}"}"#,
            r#"{"z":3,"a":2}"#,
        ),
        (
            r#"{"z":1,"$serde_json::private::RawValue":"1"}"#,
            r#"{"z":1,"$serde_json::private::RawValue":"1"}"#,
        ),
    ] {
        let tool = format!(r#"{{"call_id":"c","fn_name":"f","fn_arguments":{input}}}"#);
        let document = JsonDocument::parse(tool.as_bytes()).unwrap();
        let raw = RawToolCall::read(document.root()).unwrap();
        assert_eq!(raw.hydrate().fn_arguments.to_string(), expected);
        assert_eq!(
            serde_json::from_str::<ToolCall>(&tool)
                .unwrap()
                .fn_arguments
                .to_string(),
            expected
        );

        let custom = format!(r#"{{"data":{input}}}"#);
        let document = JsonDocument::parse(custom.as_bytes()).unwrap();
        let raw = RawCustomPart::read(document.root()).unwrap();
        assert_eq!(raw.hydrate().data.to_string(), expected);
        assert_eq!(
            serde_json::from_str::<CustomPart>(&custom)
                .unwrap()
                .data
                .to_string(),
            expected
        );
    }
}

#[test]
fn record_json_foreign_raw_value_sentinel_failures_are_typed_at_the_source_field() {
    let text = br#"{"data":{"$serde_json::private::RawValue":"["}}"#;
    let document = JsonDocument::parse(text).unwrap();
    let error = RawCustomPart::read(document.root()).unwrap_err();
    assert_eq!(
        error.path,
        vec![
            DataStep::Field("data".into()),
            DataStep::Field("$serde_json::private::RawValue".into())
        ]
    );
    assert!(matches!(error.cause.as_ref(), DecodeCause::JsonSyntax(_)));

    let text =
        br#"{"call_id":"c","fn_name":"f","fn_arguments":{"$serde_json::private::RawValue":0}}"#;
    let document = JsonDocument::parse(text).unwrap();
    let error = RawToolCall::read(document.root()).unwrap_err();
    assert_eq!(
        error.path,
        vec![
            DataStep::Field("fn_arguments".into()),
            DataStep::Field("$serde_json::private::RawValue".into())
        ]
    );
    assert!(matches!(error.cause.as_ref(), DecodeCause::JsonShape(_)));

    let document =
        JsonDocument::parse(br#"{"$serde_json::private::RawValue":"1","later":2}"#).unwrap();
    assert!(matches!(
        RawForeignValue::read(document.root())
            .unwrap_err()
            .cause
            .as_ref(),
        DecodeCause::Length(_)
    ));
    let document = JsonDocument::parse(br#"{"$serde_json::private::RawValue":"[1e400]"}"#).unwrap();
    let error = RawForeignValue::read(document.root()).unwrap_err();
    assert_eq!(
        error.path,
        vec![
            DataStep::Field("$serde_json::private::RawValue".into()),
            DataStep::Index(0u64.into())
        ]
    );
    assert!(matches!(error.cause.as_ref(), DecodeCause::JsonNumber(_)));
}
