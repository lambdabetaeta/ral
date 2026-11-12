use super::*;
use crate::data::datum::{Datum, tag};
use crate::data::json::{self, drop_value, from_slice, to_vec, to_vec_pretty};
use crate::error::Error;
use crate::fact::*;
use crate::path::EnginePath;
use crate::process::RootCause;
use crate::protocol::Ending;
use crate::protocol::probe::Probe;
use crate::shell::grant::SpawnGrant;
use crate::source::CallSite;
use serde_json::json;

fn failure<T: JsonDecode + std::fmt::Debug>(bytes: &[u8]) -> DecodeFailure {
    let JsonInputFailure::Decode(error) = from_slice::<T>(bytes).unwrap_err() else {
        panic!("valid JSON syntax")
    };
    error
}

fn encode_raw<T: Serialize>(raw: &T) -> Vec<u8> {
    serialize_structural(raw, JsonFormat::Compact)
}

fn attach() -> Attach {
    Attach::new(
        "recipe",
        EnginePath::Host("work".into()),
        EnginePath::Host("home".into()),
    )
}

fn run() -> Run {
    Run::captured(
        Program::Hook {
            name: crate::shell::runtime::HookName::session("hook"),
            args: vec![FOValue::Map {
                entries: vec![
                    ("same".into(), FOValue::Int { value: 1 }),
                    ("same".into(), FOValue::Int { value: 2 }),
                ],
            }],
        },
        crate::source::SourceOrigin::named("<wire>"),
    )
}

fn ran(ending: Ending) -> Ran {
    Ran {
        ending,
        rendered: "report text".into(),
        single_command: false,
        captured: Some(Captured::default()),
        trail: vec![FOValue::Bytes {
            value: vec![0, 255],
        }],
    }
}

fn wire(reason: Reason) -> WireError {
    WireError::of(
        &Error::new(reason)
            .with_context("inner")
            .with_context("outer")
            .with_hints(["first", "second", "first", ""]),
    )
}

fn reasons() -> Vec<Reason> {
    vec![
        Reason::process("missing", Failure::NotFound),
        Reason::process(
            "denied",
            Failure::not_runnable("denied", Unrunnable::PermissionDenied),
        ),
        Reason::TimedOut,
        Reason::Cancelled,
        Reason::raised("authored failure"),
        Reason::Denied(Box::new(Denial::exec_run(
            "git".into(),
            Withholding::MissingAllowance(SingleOrNested::Nested),
        ))),
        Reason::Io(Box::new(IoFailure {
            operation: IoOperation::Read,
            subject: IoSubject::Path(crate::text::quote_native(std::ffi::OsStr::new("input"))),
            cause: IoCause::new(IoLeaf::Torn),
        })),
        Reason::Invalid(Box::new(InvalidFailure {
            operation: "test".into(),
            path: vec![DataStep::Index(u64::MAX.into())],
            cause: InvalidCause::Empty,
        })),
        Reason::Evaluation(Box::new(EvalFailure::MissingBinding("binding".into()))),
        Reason::Evaluation(Box::new(EvalFailure::WithheldHandle(
            "parent worker".into(),
        ))),
        Reason::State(Box::new(StateFailure::CaptureLimit(CaptureLimit {
            bytes: u64::MAX.into(),
        }))),
        Reason::Compile(Box::new(CompileFailure {
            source: "source.ral".into(),
            phase: CompilePhase::Types(TypeErrorCount {
                count: (i64::MAX as u64 + 1).into(),
            }),
        })),
        Reason::Launch(Box::new(LaunchFailure::BatchImage("build.cmd".into()))),
        Reason::Protocol(Box::new(ProtocolFailure::UnknownInstaller("recipe".into()))),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Boot(
            BootFailure::AlreadyBooted,
        )))),
        Reason::Invariant(Box::new(InvariantFailure::MissingEngineFactory)),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Keybinding(
            KeybindingFailure::Notation(KeybindingFailureNotation {
                key: "shift-alt-ß".into(),
                cause: KeyNotationFailure::ShiftedCharacter(KeyNotationFailureShiftedCharacter {
                    chord: "shift-alt-ß".into(),
                    character: "ß".into(),
                    control: false,
                    alt: true,
                }),
            }),
        )))),
        Reason::Invalid(Box::new(InvalidFailure {
            operation: "load-plugin".into(),
            path: vec![DataStep::Field("zoxide".into())],
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Map),
                found: Found::Int(7),
            }),
        })),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Editor(
            EditorFailure::Command(EditorFailureCommand {
                variable: EditorVariable::Visual,
                cause: EditorCommandCause::Unbalanced,
            }),
        )))),
        Reason::State(Box::new(StateFailure::NurseryMissing(u64::MAX.into()))),
    ]
}

#[test]
fn every_reason_family_keeps_reason_description_code_and_ordered_report_facts() {
    for reason in reasons() {
        for context in [None, Some(String::new()), Some("outer: inner".into())] {
            let refusal = WireError {
                reason: reason.clone(),
                context,
                hints: vec!["one".into(), "two".into(), "one".into(), String::new()],
            };
            for bytes in [to_vec(&refusal).unwrap(), to_vec_pretty(&refusal).unwrap()] {
                let back: WireError = from_slice(&bytes).unwrap();
                assert_eq!(back, refusal);
                let error = back.to_error();
                assert_eq!(error.reason, reason);
                assert_eq!(error.describe(), reason.describe());
                assert_eq!(error.reason.advice(), reason.advice());
                let original = refusal.to_error();
                assert_eq!(error.plain(), original.plain());
                assert_eq!(error.failure_code(), reason.failure_code());
                assert_eq!(error.context(), refusal.context.as_deref());
                assert_eq!(
                    error
                        .hints()
                        .iter()
                        .map(AsRef::as_ref)
                        .collect::<Vec<&str>>(),
                    ["one", "two", "one", ""]
                );
                assert_eq!(WireError::of(&error), refusal);
            }
        }
    }
}

#[test]
fn wire_normalization_discards_only_host_provenance_witness_and_unwind() {
    let mut error = Error::new(Reason::State(Box::new(StateFailure::NoDesk)))
        .with_context("enquire")
        .with_hints(["first", "second"])
        .with_witness(Some(crate::source::Span::new(
            crate::source::FileId::DUMMY,
            0,
            1,
        )))
        .abandoning_tail()
        .discarded(17);
    error.prov = Some(crate::error::Prov::Live {
        name: "host".into(),
        span: crate::source::Span::new(crate::source::FileId::DUMMY, 0, 1),
    });
    let back = WireError::of(&error).to_error();
    assert_eq!(back.reason, error.reason);
    assert_eq!(back.context(), error.context());
    assert_eq!(back.hints(), error.hints());
    assert!(back.prov.is_none());
    assert!(back.witness().is_none());
    assert!(back.rejection().is_none());
    assert_eq!(back.unwound(), crate::error::Unwound::default());
    assert_eq!(WireError::of(&back).message(), back.to_string());
}

#[test]
fn wire_error_has_exact_datum_reason_envelope_and_no_old_message_fallback() {
    let refusal = wire(Reason::TimedOut);
    assert_eq!(
        String::from_utf8(to_vec(&refusal).unwrap()).unwrap(),
        r#"{"reason":{"variant":{"label":"timed-out","payload":null}},"context":"outer: inner","hints":["first","second","first",""]}"#
    );
    assert_eq!(
        failure::<WireError>(br#"{"message":"old","hints":[]}"#),
        DecodeFailure::new(DecodeCause::MissingField("reason".into()))
    );
    assert_eq!(
        failure::<WireError>(br#"{"reason":"unit","reason":"unit","hints":[]}"#),
        DecodeFailure::new(DecodeCause::DuplicateField("reason".into()))
    );
    assert_eq!(
        failure::<WireError>(br#"{"reason":{"variant":{"label":"timed-out","payload":null}}}"#),
        DecodeFailure::new(DecodeCause::MissingField("hints".into()))
    );
    assert_eq!(
        from_slice::<WireError>(br#"[{"variant":{"label":"timed-out","payload":null}},null,[]]"#)
            .unwrap(),
        WireError {
            reason: Reason::TimedOut,
            context: None,
            hints: vec![]
        }
    );
    assert_eq!(from_slice::<WireError>(br#"{"ignored":1,"reason":{"variant":{"label":"timed-out","payload":null}},"hints":[],"ignored":2}"#).unwrap(), WireError { reason: Reason::TimedOut, context: None, hints: vec![] });
}

#[test]
fn malformed_reason_retains_logical_datum_paths_without_json_wrapper_paths() {
    let raw = RawFrame::Answer(
        EnquiryId(7),
        Err(RawWireError {
            reason: RawDatum::new(RawFOValue::from_value(tag(
                "compile",
                Some(FOValue::Map {
                    entries: vec![
                        (
                            "source".into(),
                            FOValue::String {
                                value: "module".into(),
                            },
                        ),
                        (
                            "phase".into(),
                            tag(
                                "types",
                                Some(FOValue::Map {
                                    entries: vec![(
                                        "count".into(),
                                        FOValue::Map {
                                            entries: vec![
                                                ("high".into(), FOValue::Int { value: -1 }),
                                                ("low".into(), FOValue::Int { value: 0 }),
                                            ],
                                        },
                                    )],
                                }),
                            ),
                        ),
                    ],
                }),
            ))),
            context: None,
            hints: vec![],
        }),
    );
    let error = failure::<Frame>(&encode_raw(&raw));
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Answer".into()),
            DataStep::Index(1usize.into()),
            DataStep::Payload("Err".into()),
            DataStep::Field("reason".into()),
            DataStep::Payload("compile".into()),
            DataStep::Field("phase".into()),
            DataStep::Payload("types".into()),
            DataStep::Field("count".into()),
            DataStep::Field("high".into()),
        ]
    );
    assert_eq!(
        *error.cause,
        DecodeCause::IntegerRange(DecodeIntegerRange {
            value: -1,
            target: DecodeIntegerTarget::U32
        })
    );
}

#[test]
fn duplicate_reason_fields_survive_raw_map_pairs_until_datum_validation() {
    let mut value = CompileFailure {
        source: "module".into(),
        phase: CompilePhase::Parse,
    }
    .encode();
    let FOValue::Map { entries } = &mut value else {
        panic!("record")
    };
    entries.push(("source".into(), "duplicate".to_owned().encode()));
    let raw = RawWireError {
        reason: RawDatum::new(RawFOValue::from_value(tag("compile", Some(value)))),
        context: None,
        hints: vec![],
    };
    let error = failure::<WireError>(&encode_raw(&raw));
    assert_eq!(
        error.path,
        [
            DataStep::Field("reason".into()),
            DataStep::Payload("compile".into())
        ]
    );
    assert_eq!(*error.cause, DecodeCause::DuplicateField("source".into()));
}

#[test]
fn error_record_and_wire_envelopes_preserve_the_same_reason_but_not_the_same_provenance() {
    for reason in reasons() {
        let record = ErrorFact::new(
            reason.clone(),
            "supplied-frame".into(),
            CallSite {
                script: "supplied.ral".into(),
                line: 12,
                col: 8,
            },
        );
        let report = Report::Ran(ran(Ending::Err(record.clone())));
        let back: Report = from_slice(&to_vec(&report).unwrap()).unwrap();
        let Report::Ran(Ran {
            ending: Ending::Err(back),
            ..
        }) = back
        else {
            panic!("err report")
        };
        assert_eq!(back, record);
        assert_eq!(back.reason(), &wire(reason.clone()).to_error().reason);
        assert_eq!(back.reason().describe(), reason.describe());
        assert_eq!(back.reason().advice(), reason.advice());
        assert_eq!(back.frame(), "supplied-frame");
        assert_eq!(back.site(), record.site());
        assert!(wire(reason).to_error().prov.is_none());
    }
}

#[test]
fn every_frame_and_event_arm_preserves_successful_compact_pretty_shapes() {
    let refusal = wire(Reason::Protocol(Box::new(
        ProtocolFailure::UnknownInstaller("recipe".into()),
    )));
    let frames = vec![
        Frame::Attach(attach()),
        Frame::Detach,
        Frame::Dispatch(DispatchId(u64::MAX), Box::new(run())),
        Frame::Probe(DispatchId(1), Probe::PathEntries("directory".into())),
        Frame::Event(DispatchId(1), Event::Surface(FOValue::Unit)),
        Frame::Event(
            DispatchId(1),
            Event::Enquiry(
                EnquiryId(u64::MAX),
                FOValue::Bytes {
                    value: vec![0, 255],
                },
            ),
        ),
        Frame::Event(DispatchId(1), Event::Reading(Ok(FOValue::Unit))),
        Frame::Event(DispatchId(1), Event::Reading(Err(ProbeFailure::Busy))),
        Frame::Event(
            DispatchId(1),
            Event::Report(Report::Ran(ran(Ending::Ok(FOValue::Unit)))),
        ),
        Frame::Session(SessionEvent::Attached),
        Frame::Session(SessionEvent::Ended),
        Frame::Session(SessionEvent::Refused(refusal.clone())),
        Frame::Session(SessionEvent::DeferredSurface(vec![FOValue::Unit])),
        Frame::Control(Control::Strike(DispatchId(1), CancelCause::TimedOut)),
        Frame::Control(Control::End(RootCause::Cancelled)),
        Frame::Answer(EnquiryId(1), Ok(FOValue::Unit)),
        Frame::Answer(EnquiryId(1), Err(refusal)),
        Frame::Ping(u64::MAX),
        Frame::Pong(u64::MAX),
    ];
    for frame in frames {
        assert_eq!(to_vec(&frame).unwrap(), serde_json::to_vec(&frame).unwrap());
        assert_eq!(
            to_vec_pretty(&frame).unwrap(),
            serde_json::to_vec_pretty(&frame).unwrap()
        );
        assert_eq!(
            from_slice::<Frame>(&to_vec(&frame).unwrap()).unwrap(),
            frame
        );
        assert_eq!(
            from_slice::<Frame>(&to_vec_pretty(&frame).unwrap()).unwrap(),
            frame
        );
    }
}

#[test]
fn malformed_frame_shape_tag_tuple_and_missing_fields_are_typed() {
    assert_eq!(
        failure::<Frame>(br#"{"Dispatch":[1]}"#).path,
        [DataStep::Payload("Dispatch".into())]
    );
    assert!(matches!(
        *failure::<Frame>(br#"{"Dispatch":[1]}"#).cause,
        DecodeCause::Length(_)
    ));
    assert_eq!(
        failure::<Frame>(br#"{"Attach":{}}"#),
        DecodeFailure::new(DecodeCause::MissingField("engine".into())).payload("Attach")
    );
    assert!(matches!(
        *failure::<Frame>(br#"{"Teleport":1}"#).cause,
        DecodeCause::UnknownTag(_)
    ));
    assert!(matches!(
        *failure::<Frame>(b"0").cause,
        DecodeCause::JsonShape(_)
    ));
    assert!(matches!(
        from_slice::<Frame>(b"{").unwrap_err(),
        JsonInputFailure::Syntax(_)
    ));
    assert!(matches!(
        *failure::<Frame>(br#"{"Ping":1,"Pong":2}"#).cause,
        DecodeCause::Length(_)
    ));
    let error = failure::<Frame>(br#"{"Ping":18446744073709551616}"#);
    assert_eq!(error.path, [DataStep::Payload("Ping".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::JsonNumber(JsonNumberFailure {
            text: "18446744073709551616".into(),
            target: JsonNumberTarget::U64,
            cause: JsonNumberFault::OutOfRange
        })
    );
}

#[test]
fn raw_attach_preserves_positional_optional_unknown_and_duplicate_field_policy() {
    let raw = RawAttach::from_value(&attach()).unwrap();
    let engine = encode_raw(&raw.engine);
    let origin = encode_raw(&raw.origin);
    let engine = String::from_utf8(engine).unwrap();
    let origin = String::from_utf8(origin).unwrap();
    let positional = format!("[{engine},{origin}]");
    assert_eq!(
        from_slice::<Attach>(positional.as_bytes()).unwrap(),
        attach()
    );
    let unknown =
        format!("{{\"ignored\":1,\"engine\":{engine},\"origin\":{origin},\"ignored\":2}}");
    assert_eq!(from_slice::<Attach>(unknown.as_bytes()).unwrap(), attach());
    let duplicate = format!("{{\"engine\":{engine},\"engine\":{engine},\"origin\":{origin}}}");
    assert_eq!(
        failure::<Attach>(duplicate.as_bytes()),
        DecodeFailure::new(DecodeCause::DuplicateField("engine".into()))
    );
}

#[test]
fn root_environment_reserved_names_and_duplicate_replacement_keep_typed_paths() {
    let raw = RawRoot::from_value(&Root::new(EnginePath::Host("work".into()))).unwrap();
    let mut value = serde_json::to_value(&raw).unwrap();
    value["env"] = json!({"PWD":"forbidden"});
    let error = failure::<Root>(&serde_json::to_vec(&value).unwrap());
    assert_eq!(error.path, [DataStep::Field("env".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::ReservedEnvironment(ReservedEnvironmentFailure {
            name: "PWD".into(),
            reserved: ReservedVar::Pwd
        })
    );
    let bytes =
        br#"{"cwd":{"Host":"work"},"grants":[],"env":{"A":"first","A":"last"},"config":"unit"}"#;
    let back: Root = from_slice(bytes).unwrap();
    assert_eq!(
        back.env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        vec![("A", "last")]
    );
}

#[test]
fn finite_base64_and_present_value_paths_cross_all_frame_data_envelopes() {
    let bad = || RawFOValue::Float {
        value: f64::NAN.to_bits(),
    };
    let cases = [
        (
            RawFrame::Answer(EnquiryId(1), Ok(bad())),
            vec![
                DataStep::Payload("Answer".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload("Ok".into()),
            ],
        ),
        (
            RawFrame::Event(DispatchId(1), RawEvent::Surface(bad())),
            vec![
                DataStep::Payload("Event".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload("Surface".into()),
            ],
        ),
        (
            RawFrame::Event(DispatchId(1), RawEvent::Enquiry(EnquiryId(1), bad())),
            vec![
                DataStep::Payload("Event".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload("Enquiry".into()),
                DataStep::Index(1usize.into()),
            ],
        ),
        (
            RawFrame::Event(DispatchId(1), RawEvent::Reading(Ok(bad()))),
            vec![
                DataStep::Payload("Event".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload("Reading".into()),
                DataStep::Payload("Ok".into()),
            ],
        ),
        (
            RawFrame::Session(RawSessionEvent::DeferredSurface(vec![
                RawFOValue::Unit,
                bad(),
            ])),
            vec![
                DataStep::Payload("Session".into()),
                DataStep::Payload("DeferredSurface".into()),
                DataStep::Index(1usize.into()),
            ],
        ),
    ];
    for (raw, mut prefix) in cases {
        prefix.extend([
            DataStep::Payload("float".into()),
            DataStep::Field("value".into()),
        ]);
        let error = failure::<Frame>(&encode_raw(&raw));
        assert_eq!(error.path, prefix);
        assert_eq!(
            *error.cause,
            DecodeCause::FiniteBits(FiniteBitsFailure {
                bits: f64::NAN.to_bits().into()
            })
        );
    }
    let error = failure::<Frame>(br#"{"Event":[1,{"Surface":{"bytes":{"value":"!"}}}]}"#);
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Event".into()),
            DataStep::Index(1usize.into()),
            DataStep::Payload("Surface".into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::Base64(_)));
}

#[test]
fn program_and_report_trail_keep_field_index_payload_paths() {
    let mut raw = RawRun::from_value(&run());
    let RawProgram::Hook { args, .. } = &mut raw.program else {
        panic!("hook")
    };
    args.push(RawFOValue::Bytes { value: "!".into() });
    let error = failure::<Run>(&encode_raw(&raw));
    assert_eq!(
        error.path,
        [
            DataStep::Field("program".into()),
            DataStep::Payload("Hook".into()),
            DataStep::Field("args".into()),
            DataStep::Index(1usize.into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into())
        ]
    );
    let mut raw = RawRan::from_value(&ran(Ending::Ok(FOValue::Unit)));
    raw.trail.push(RawFOValue::Bytes { value: "!".into() });
    let error = failure::<Report>(&encode_raw(&RawReport::Ran(raw)));
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Ran".into()),
            DataStep::Field("trail".into()),
            DataStep::Index(1usize.into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into())
        ]
    );
}

#[test]
fn report_failure_status_zero_is_domain_failure_and_outside_byte_is_numeric_failure() {
    for (tag, trail) in [("Static", r#", "trail":[]"#), ("Faulted", "")] {
        let error = failure::<Report>(
            format!(r#"{{"{tag}":{{"rendered":"report","status":0{trail}}}}}"#).as_bytes(),
        );
        assert_eq!(
            error.path,
            [
                DataStep::Payload(tag.into()),
                DataStep::Field("status".into())
            ]
        );
        assert_eq!(
            *error.cause,
            DecodeCause::IntegerRange(DecodeIntegerRange {
                value: 0,
                target: DecodeIntegerTarget::FailureStatus
            })
        );
        let error = failure::<Report>(
            format!(r#"{{"{tag}":{{"rendered":"report","status":256{trail}}}}}"#).as_bytes(),
        );
        assert_eq!(
            error.path,
            [
                DataStep::Payload(tag.into()),
                DataStep::Field("status".into())
            ]
        );
        assert_eq!(
            *error.cause,
            DecodeCause::JsonNumber(JsonNumberFailure {
                text: "256".into(),
                target: JsonNumberTarget::U8,
                cause: JsonNumberFault::OutOfRange
            })
        );
    }
}

#[test]
fn run_duration_normalization_overflow_and_strict_unknown_field_policy_survive() {
    let mut raw = RawRun::from_value(&run());
    raw.wall = Some(RawDuration {
        secs: 1,
        nanos: 1_500_000_000,
    });
    let back: Run = from_slice(&encode_raw(&raw)).unwrap();
    assert_eq!(back.wall, Some(Duration::new(2, 500_000_000)));
    raw.wall = Some(RawDuration {
        secs: u64::MAX,
        nanos: 1_000_000_000,
    });
    let error = failure::<Run>(&encode_raw(&raw));
    assert_eq!(error.path, [DataStep::Field("wall".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::DurationOverflow(DurationOverflow {
            seconds: u64::MAX.into(),
            nanos: 1_000_000_000
        })
    );
    let mut value = serde_json::to_value(RawRun::from_value(&run())).unwrap();
    value["cleanup_grace"]["unknown"] = json!(true);
    let error = failure::<Run>(&serde_json::to_vec(&value).unwrap());
    assert_eq!(error.path, [DataStep::Field("cleanup_grace".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::UnknownField(UnknownField {
            field: "unknown".into(),
            offered: vec!["secs".into(), "nanos".into()]
        })
    );
}

#[test]
fn probes_round_trip_every_available_arm_and_preserve_portable_subjects() {
    let mut probes = vec![
        Probe::BindingCount,
        Probe::LeasedBindingCount,
        Probe::LargestBindingBytes,
        Probe::EnvVar("A".into()),
        Probe::Cwd,
        Probe::Home,
        Probe::BuiltinNames,
        Probe::PathBytes("a/b".into()),
        Probe::Workers,
        Probe::SessionEnded,
        Probe::CompletionNames,
        Probe::Bindings,
        Probe::PathEntries("a/b".into()),
        Probe::Grants,
    ];
    #[cfg(feature = "test-util")]
    probes.extend([Probe::WorkerCount, Probe::GrantDepth]);
    for probe in probes {
        let bytes = to_vec(&probe).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&probe).unwrap());
        let back: Probe = from_slice(&bytes).unwrap();
        assert_eq!(back.subject(), probe.subject());
        assert_eq!(back, probe);
    }
    // Portable facts remain decodable even without test-util producers.
    for subject in [ProbeSubject::WorkerCount, ProbeSubject::GrantDepth] {
        assert_eq!(
            ProbeSubject::decode(&subject.clone().encode()).unwrap(),
            subject
        );
    }
}

#[test]
fn typed_probe_refusals_keep_all_causes_in_the_existing_datum_convention() {
    let causes = [
        ProbeFailure::Busy,
        ProbeFailure::WorkerPanicked,
        ProbeFailure::Unencodable(FrameEncodeFailure::TooLarge(FrameEncodeFailureTooLarge {
            actual: u64::MAX.into(),
            limit: 100u64.into(),
        })),
    ];
    for cause in causes {
        let event = Event::Reading(Err(cause.clone()));
        let bytes = to_vec(&event).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&event).unwrap());
        assert_eq!(from_slice::<Event>(&bytes).unwrap(), event);
        let RawEvent::Reading(Err(raw)) =
            decode_structural(json::JsonDocument::parse(&bytes).unwrap().root()).unwrap()
        else {
            panic!("reading")
        };
        assert_eq!(raw.validate().unwrap(), cause);
    }
    let raw = RawEvent::Reading(Err(RawDatum::new(RawFOValue::from_value(tag(
        "unknown-probe-failure",
        None,
    )))));
    let error = failure::<Event>(&encode_raw(&raw));
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Reading".into()),
            DataStep::Payload("Err".into())
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::UnknownTag(_)));
}

#[test]
fn authored_spawn_grants_remain_unresolved_and_keep_map_pair_duplicates() {
    let values = [
        SpawnGrant::Inherit,
        SpawnGrant::Base("unknown base".into()),
        SpawnGrant::Restrict(FOValue::Map {
            entries: vec![
                ("same".into(), FOValue::Unit),
                ("same".into(), FOValue::Bool { value: true }),
            ],
        }),
    ];
    for value in values {
        assert_eq!(to_vec(&value).unwrap(), serde_json::to_vec(&value).unwrap());
        assert_eq!(
            from_slice::<SpawnGrant>(&to_vec(&value).unwrap()).unwrap(),
            value
        );
    }
    let error = failure::<SpawnGrant>(br#"{"Restrict":{"float":{"value":9218868437227405312}}}"#);
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Restrict".into()),
            DataStep::Payload("float".into()),
            DataStep::Field("value".into())
        ]
    );
    assert_eq!(
        *error.cause,
        DecodeCause::FiniteBits(FiniteBitsFailure {
            bits: f64::INFINITY.to_bits().into()
        })
    );
}

#[test]
fn a_child_seal_stays_opaque_until_the_typed_seed_endpoint_unseals_it() {
    let shell = crate::test_helper::core_shell();
    let sealed = crate::seed::seal(&shell.bequeath(SpawnGrant::Inherit)).unwrap();
    let attach = Attach::child(attach().engine, sealed);
    let back: Attach = from_slice(&to_vec(&attach).unwrap()).unwrap();
    assert_eq!(back, attach);
    let Origin::Child(sealed) = back.origin else {
        panic!("child")
    };
    let seed = sealed.unseal().unwrap();
    let bytes = to_vec(&seed).unwrap();
    let back: crate::seed::EngineSeed = from_slice(&bytes).unwrap();
    assert_eq!(to_vec(&back).unwrap(), bytes);
    let error = failure::<Origin>(br#"{"Child":"!"}"#);
    assert_eq!(error.path, [DataStep::Payload("Child".into())]);
    assert!(matches!(*error.cause, DecodeCause::Base64(_)));
}

#[test]
fn grant_stack_datum_ingress_unwraps_syntax_and_preserves_typed_nested_paths() {
    use crate::authority::policy::GrantStack;
    let syntax = GrantStack::decode(&crate::data::Bytes::from(b"[".to_vec()).encode()).unwrap_err();
    assert_eq!(syntax.path, [] as [DataStep; 0]);
    assert!(matches!(*syntax.cause, DecodeCause::JsonSyntax(_)));
    let bytes = br#"[{"exec":{"allows":[{"File":{"written":"literal","real":{"Unix":[0]},"found_as":null}}],"denies":[]}}]"#;
    // The exact host's lexical adapter decides the domain cause; Datum must
    // preserve that cause and its full path rather than stringify it.
    let JsonInputFailure::Decode(direct) = from_slice::<GrantStack>(bytes).unwrap_err() else {
        panic!("decode")
    };
    let datum = GrantStack::decode(&crate::data::Bytes::from(bytes.to_vec()).encode()).unwrap_err();
    assert_eq!(datum, direct);
    assert_eq!(
        datum.path,
        [
            DataStep::Index(0usize.into()),
            DataStep::Field("exec".into()),
            DataStep::Field("allows".into()),
            DataStep::Index(0usize.into()),
            DataStep::Payload("File".into()),
            DataStep::Field("real".into())
        ]
    );
    #[derive(Debug)]
    struct Envelope {
        stack: GrantStack,
    }
    crate::record!(Envelope { stack: "stack" });
    let error = Envelope::decode(&FOValue::Map {
        entries: vec![(
            "stack".into(),
            crate::data::Bytes::from(bytes.to_vec()).encode(),
        )],
    })
    .unwrap_err();
    assert_eq!(error, direct.field("stack"));
    let stack = GrantStack::root();
    assert_eq!(GrantStack::decode(&stack.clone().encode()).unwrap(), stack);
}

#[test]
fn non_unicode_paths_are_typed_at_every_protocol_path_ancestor() {
    #[cfg(unix)]
    let bad = || {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]))
    };
    #[cfg(windows)]
    let bad = || {
        use std::os::windows::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0xd800]))
    };
    for (probe, tag) in [
        (Probe::PathBytes(bad()), "PathBytes"),
        (Probe::PathEntries(bad()), "PathEntries"),
    ] {
        let error = to_vec(&Frame::Probe(DispatchId(1), probe)).unwrap_err();
        assert_eq!(
            error.path,
            [
                DataStep::Payload("Probe".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload(tag.into())
            ]
        );
        assert!(matches!(error.cause, JsonEncodeCause::NonUnicodePath(_)));
    }
    let mut home_attach = attach();
    home_attach.engine.home = EnginePath::Host(bad());
    let error = to_vec(&Frame::Attach(home_attach)).unwrap_err();
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Attach".into()),
            DataStep::Field("engine".into()),
            DataStep::Field("home".into()),
            DataStep::Payload("Host".into())
        ]
    );
    let mut attach = attach();
    let Origin::Root(root) = &mut attach.origin else {
        panic!("root")
    };
    root.cwd = EnginePath::Host(bad());
    let error = to_vec(&Frame::Attach(attach)).unwrap_err();
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Attach".into()),
            DataStep::Field("origin".into()),
            DataStep::Payload("Root".into()),
            DataStep::Field("cwd".into()),
            DataStep::Payload("Host".into())
        ]
    );
}

#[test]
fn completed_deep_data_prefixes_are_drained_when_later_elements_or_durations_fail() {
    std::thread::Builder::new().stack_size(256*1024).spawn(|| {
        let deep = format!("{}\"unit\"{}", r#"{"list":{"items":["#.repeat(10_000), "]}}".repeat(10_000));
        for bytes in [
            format!(r#"{{"Session":{{"DeferredSurface":[{deep},{{"bytes":{{"value":"!"}}}}]}}}}"#),
            format!(r#"{{"Event":[1,{{"Report":{{"Ran":{{"ending":{{"ok":{deep}}},"rendered":"","single_command":false,"captured":null,"trail":[{{"bytes":{{"value":"!"}}}}]}}}}}}]}}"#),
        ] {
            assert!(matches!(from_slice::<Frame>(bytes.as_bytes()), Err(JsonInputFailure::Decode(_))));
        }
        let mut value = FOValue::Unit;
        for _ in 0..10_000 { value = FOValue::List { items: vec![value] }; }
        let program = Program::Hook { name: crate::shell::runtime::HookName::session("deep"), args: vec![value] };
        let mut run = Run::captured(program, crate::source::SourceOrigin::named("<deep>"));
        let mut raw = RawRun::from_value(&run);
        let Program::Hook { args, .. } = &mut run.program else { panic!("hook") };
        for value in std::mem::take(args) { drop_value(value); }
        raw.wall = Some(RawDuration { secs: u64::MAX, nanos: 1_000_000_000 });
        assert!(matches!(from_slice::<Run>(&encode_raw(&raw)), Err(JsonInputFailure::Decode(_))));
    }).unwrap().join().unwrap();
}

#[test]
fn raw_datum_endpoints_retain_absent_explicit_unit_and_error_record_field_paths() {
    for reason in [
        r#"{"variant":{"label":"timed-out"}}"#,
        r#"{"variant":{"label":"timed-out","payload":"unit"}}"#,
    ] {
        let bytes = format!(r#"{{"reason":{reason},"hints":[]}}"#);
        let back: WireError = from_slice(bytes.as_bytes()).unwrap();
        assert_eq!(back.reason, Reason::TimedOut);
        assert_eq!(
            json::from_datum_slice::<Reason>(reason.as_bytes()).unwrap(),
            Reason::TimedOut
        );
    }
    let mut value = ErrorFact::new(
        Reason::Cancelled,
        "original".into(),
        crate::test_helper::site(),
    )
    .encode();
    let FOValue::Map { entries } = &mut value else {
        panic!("record")
    };
    entries
        .iter_mut()
        .find(|(key, _)| key == "frame")
        .unwrap()
        .1 = FOValue::Int { value: 3 };
    let raw = RawReport::Ran(RawRan {
        ending: crate::fact::Ending::Err(RawDatum::new(RawFOValue::from_value(value))),
        rendered: String::new(),
        single_command: false,
        captured: None,
        trail: vec![],
    });
    let error = failure::<Report>(&encode_raw(&raw));
    assert_eq!(
        error.path,
        [
            DataStep::Payload("Ran".into()),
            DataStep::Field("ending".into()),
            DataStep::Payload("err".into()),
            DataStep::Field("frame".into())
        ]
    );
    assert_eq!(
        *error.cause,
        DecodeCause::Shape(ShapeFailure {
            expected: Expected::Shape(ValueShape::String),
            found: Found::Int(3)
        })
    );
}

#[test]
fn every_admitted_root_rejects_a_wrong_json_shape_without_a_semantic_serde_fallback() {
    macro_rules! root {
        ($type:ty) => {
            assert!(matches!(
                *failure::<$type>(b"0").cause,
                DecodeCause::JsonShape(_)
            ));
        };
    }
    root!(Frame);
    root!(Attach);
    root!(EngineSpec);
    root!(Origin);
    root!(Root);
    root!(Run);
    root!(Program);
    root!(Event);
    root!(SessionEvent);
    root!(Report);
    root!(Ran);
    root!(WireError);
    root!(Ending);
    root!(Control);
    root!(Probe);
    root!(SpawnGrant);
    root!(crate::authority::policy::GrantStack);
    root!(crate::seed::EngineSeed);
    root!(crate::seed::Sealed);
    root!(FOValue);
    assert!(matches!(
        json::from_datum_slice::<Reason>(b"\"unit\""),
        Err(JsonInputFailure::Decode(DecodeFailure {
            cause,
            ..
        })) if matches!(cause.as_ref(), DecodeCause::Shape(_))
    ));
    assert!(matches!(
        json::from_datum_slice::<ErrorFact>(b"\"unit\""),
        Err(JsonInputFailure::Decode(DecodeFailure {
            cause,
            ..
        })) if matches!(cause.as_ref(), DecodeCause::Shape(_))
    ));
}
