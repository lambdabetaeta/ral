use super::*;
use crate::data::json::{drop_value, from_slice, to_vec, to_vec_pretty};
use crate::fact::{
    Count, DataStep, DecodeCause, FrameEncodeFailure, FrameFailure, FramePart, InvariantFailure,
    JsonEncodeCause, JsonInputFailure, ProtocolFailure, ProtocolOperation, Reason,
    SeedFrameFailure,
};
use crate::ir::CompKind;
use crate::seed::WireDecoder;
use crate::seed::table::{SerialNative, SerialThunk};
use crate::source::Spanned;
use serde_json::json;

fn seed() -> EngineSeed {
    EngineSeed::encode(&crate::test_helper::core_shell().bequeath(SpawnGrant::Inherit)).unwrap()
}

fn envelope() -> serde_json::Value {
    serde_json::from_slice(&to_vec(&seed()).unwrap()).unwrap()
}

fn value_failure(value: &serde_json::Value) -> DecodeFailure {
    let JsonInputFailure::Decode(error) =
        from_slice::<EngineSeed>(&serde_json::to_vec(value).unwrap()).unwrap_err()
    else {
        panic!("valid JSON syntax")
    };
    error
}

fn thunk(bits: u64, bindings: u32) -> serde_json::Value {
    json!({"ext": {"Thunk": {
        "comp": {"span": null, "item": {"Return": {"Float": bits}}},
        "env": {"bindings": bindings}
    }}})
}

fn with_binding(mut value: serde_json::Value, binding: &serde_json::Value) -> serde_json::Value {
    value["scope_table"] = json!([[["bound", {"value": binding, "scheme": null}]]]);
    value["env"] = json!({"bindings": 0});
    value
}

fn table_path() -> Vec<DataStep> {
    vec![
        DataStep::Field("scope_table".into()),
        DataStep::Index(0usize.into()),
        DataStep::Index(0usize.into()),
        DataStep::Index(1usize.into()),
        DataStep::Field("value".into()),
    ]
}

#[test]
fn raw_engine_seed_preserves_successful_json_and_safe_metadata() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_var(
        "finite".into(),
        crate::Value::Float(crate::data::Finite::new(-0.0).unwrap()),
    );
    shell.install_script_context(
        &crate::source::SourceOrigin::Virtual("seed-source".into()),
        "echo source\n",
    );
    let seed = EngineSeed::encode(&shell.bequeath(SpawnGrant::Base("unresolved".into()))).unwrap();
    let compact = to_vec(&seed).unwrap();
    let pretty = to_vec_pretty(&seed).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&compact).unwrap(),
        serde_json::from_slice::<serde_json::Value>(&pretty).unwrap()
    );
    for bytes in [compact, pretty] {
        let back: EngineSeed = from_slice(&bytes).unwrap();
        assert_eq!(seed.scope_table, back.scope_table);
        assert_eq!(seed.env, back.env);
        assert_eq!(seed.sources, back.sources);
        assert_eq!(seed.terms, back.terms);
        assert_eq!(seed.grant, back.grant);
        assert_eq!(
            serde_json::to_value(&seed.fixings).unwrap(),
            serde_json::to_value(&back.fixings).unwrap()
        );
    }
}

#[test]
fn raw_engine_seed_preserves_positional_structs_and_optional_fields() {
    let value = envelope();
    let fields = [
        "scope_table",
        "env",
        "context",
        "sources",
        "terms",
        "fixings",
        "grant",
    ];
    let sequence = fields.map(|field| value[field].clone());
    assert!(from_slice::<EngineSeed>(&serde_json::to_vec(&sequence).unwrap()).is_ok());
    let mut value = with_binding(value, &json!({"variant": {"label": "bare"}}));
    value["scope_table"][0][0][1]
        .as_object_mut()
        .unwrap()
        .remove("scheme");
    let seed: EngineSeed = from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(seed.scope_table[0][0].1.scheme.is_none());
    let SerialValue::Variant { payload, .. } = &seed.scope_table[0][0].1.value else {
        panic!("variant")
    };
    assert!(payload.is_none());
}

#[test]
fn raw_engine_seed_keeps_duplicate_known_and_unknown_field_policies() {
    let bytes = String::from_utf8(to_vec(&seed()).unwrap()).unwrap();
    let duplicate = bytes.replacen('{', r#"{"env":{"bindings":0},"#, 1);
    assert_eq!(
        from_slice::<EngineSeed>(duplicate.as_bytes()).unwrap_err(),
        JsonInputFailure::Decode(DecodeFailure::new(DecodeCause::DuplicateField(
            "env".into()
        )))
    );
    let ignored = bytes.replacen('{', r#"{"ignored":[false],"ignored":null,"#, 1);
    assert!(from_slice::<EngineSeed>(ignored.as_bytes()).is_ok());
    let mut missing = envelope();
    missing.as_object_mut().unwrap().remove("terms");
    assert_eq!(
        *value_failure(&missing).cause,
        DecodeCause::MissingField("terms".into())
    );
}

#[test]
fn raw_scope_table_float_path_includes_tuple_slots_and_extension_payloads() {
    let bits = f64::NEG_INFINITY.to_bits();
    let error = value_failure(&with_binding(envelope(), &thunk(bits, u32::MAX)));
    let mut expected = table_path();
    expected.extend([
        DataStep::Payload("ext".into()),
        DataStep::Payload("Thunk".into()),
        DataStep::Field("comp".into()),
        DataStep::Field("item".into()),
        DataStep::Payload("Return".into()),
        DataStep::Payload("Float".into()),
    ]);
    assert_eq!(error.path, expected);
    assert!(
        matches!(*error.cause, DecodeCause::FiniteBits(cause) if cause.bits == Count::from_u64(bits))
    );
}

#[test]
fn raw_scope_table_bytes_keep_first_order_payload_path() {
    let error = value_failure(&with_binding(envelope(), &json!({"bytes": {"value": "!"}})));
    let mut path = table_path();
    path.extend([
        DataStep::Payload("bytes".into()),
        DataStep::Field("value".into()),
    ]);
    assert_eq!(error.path, path);
    assert!(matches!(*error.cause, DecodeCause::Base64(_)));
}

#[test]
fn raw_handler_catch_all_keeps_context_and_first_order_paths() {
    let mut value = envelope();
    value["context"]["handlers"] = json!([{
        "entries": [], "kind": "Alias",
        "catch_all": {"list": {"items": [{"float": {"value": f64::NAN.to_bits()}}]}}
    }]);
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("context".into()),
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("catch_all".into()),
            DataStep::Payload("list".into()),
            DataStep::Field("items".into()),
            DataStep::Index(0usize.into()),
            DataStep::Payload("float".into()),
            DataStep::Field("value".into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::FiniteBits(_)));
}

#[test]
fn raw_spawn_grant_preserves_authored_values_and_duplicate_map_entries() {
    let raw: RawSpawnGrant = decode_structural(
        crate::data::json::JsonDocument::parse(
            br#"{"Restrict":{"map":{"entries":[["fs","unit"],["fs",{"int":{"value":3}}]]}}}"#,
        )
        .unwrap()
        .root(),
    )
    .unwrap();
    let grant = raw.validate().unwrap();
    assert_eq!(
        serialize_structural(&RawSpawnGrant::from_value(&grant), JsonFormat::Compact),
        serde_json::to_vec(&grant).unwrap()
    );
    let SpawnGrant::Restrict(crate::data::FOValue::Map { entries }) = grant else {
        panic!("authored record")
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, entries[1].0);
    let mut value = envelope();
    value["grant"] = json!({"Restrict": {"bytes": {"value": "!"}}});
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("grant".into()),
            DataStep::Payload("Restrict".into()),
            DataStep::Payload("bytes".into()),
            DataStep::Field("value".into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::Base64(_)));
}

#[test]
fn raw_seed_json_does_not_resolve_scope_references_or_native_names() {
    for binding in [
        thunk(0, u32::MAX),
        json!({"ext": {"Native": {"name": "not-a-native"}}}),
    ] {
        let value = with_binding(envelope(), &binding);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(from_slice::<EngineSeed>(&bytes).is_ok());
    }
}

#[test]
fn seed_hydration_keeps_unresolved_cyclic_and_native_causes_distinct() {
    let runtime = crate::test_helper::core_runtime();
    for (binding, expected) in [
        (
            thunk(0, u32::MAX),
            ProtocolFailure::UnresolvedScope(u64::from(u32::MAX).into()),
        ),
        (thunk(0, 0), ProtocolFailure::CyclicScopes),
        (
            json!({"ext": {"Native": {"name": "not-a-native"}}}),
            ProtocolFailure::UnknownNative("not-a-native".into()),
        ),
    ] {
        let bytes = serde_json::to_vec(&with_binding(envelope(), &binding)).unwrap();
        let error = from_slice::<EngineSeed>(&bytes)
            .unwrap()
            .decode(&runtime)
            .err()
            .expect("seed hydration must reject the invalid binding");
        assert_eq!(error.reason, Reason::Protocol(Box::new(expected)));
        assert!(error.context().is_some());
    }
}

#[test]
fn seal_preserves_original_handle_invariant_without_protocol_wrapping() {
    let mut bequest = crate::test_helper::core_shell().bequeath(SpawnGrant::Inherit);
    bequest.image.env.bind(
        "unscrubbed".into(),
        crate::shell::runtime::Binding {
            value: crate::shell::runtime::idle_handle(),
            scheme: None,
        },
    );
    assert_eq!(
        super::super::seal(&bequest).unwrap_err().reason,
        Reason::Invariant(Box::new(InvariantFailure::HandleInSeed))
    );
}

#[test]
fn unseal_distinguishes_empty_trailing_and_partial_frames() {
    assert_eq!(
        Sealed(Bytes::from(Vec::new())).unseal().unwrap_err().reason,
        Reason::Protocol(Box::new(ProtocolFailure::SeedFrame(
            SeedFrameFailure::Empty
        )))
    );
    let sealed =
        super::super::seal(&crate::test_helper::core_shell().bequeath(SpawnGrant::Inherit))
            .unwrap();
    let mut bytes = sealed.0.to_vec();
    bytes.extend([0, 1, 2]);
    let Reason::Protocol(failure) = Sealed(bytes.into()).unseal().unwrap_err().reason else {
        panic!("protocol")
    };
    assert!(
        matches!(*failure, ProtocolFailure::SeedFrame(SeedFrameFailure::Trailing(cause)) if cause.bytes == 3usize.into())
    );
    let Reason::Protocol(failure) = Sealed(Bytes::from(vec![1, 2])).unseal().unwrap_err().reason
    else {
        panic!("protocol")
    };
    assert!(
        matches!(*failure, ProtocolFailure::SeedFrame(SeedFrameFailure::Frame(FrameFailure::Partial(cause)))
        if cause.part == FramePart::Length && cause.got == 2usize.into() && cause.expected == 4usize.into())
    );
}

#[test]
fn sealed_json_retains_base64_validation_and_datum_representation() {
    let sealed = Sealed(Bytes::from(vec![0, 1, 255]));
    assert_eq!(
        to_vec(&sealed).unwrap(),
        serde_json::to_vec(&sealed).unwrap()
    );
    assert_eq!(
        from_slice::<Sealed>(&to_vec(&sealed).unwrap()).unwrap(),
        sealed
    );
    assert!(matches!(
        from_slice::<Sealed>(br#""!""#).unwrap_err(),
        JsonInputFailure::Decode(DecodeFailure {
            cause,
            ..
        }) if matches!(cause.as_ref(), DecodeCause::Base64(_))
    ));
}

fn deep_value(depth: usize) -> String {
    let mut value = String::new();
    for _ in 0..depth {
        value.push_str(r#"{"list":{"items":["#);
    }
    value.push_str(
        r#"{"ext":{"Thunk":{"comp":{"span":null,"item":{"Return":"Unit"}},"env":{"bindings":0}}}}"#,
    );
    for _ in 0..depth {
        value.push_str("]}}");
    }
    value
}

#[test]
fn raw_closure_graph_converts_and_drops_deep_data_on_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let bytes = format!(r#"{{"value":{},"scheme":null}}"#, deep_value(10_000));
            let binding: SerialBinding = from_slice(bytes.as_bytes()).unwrap();
            assert_eq!(to_vec(&binding).unwrap(), bytes.as_bytes());
            drop(binding);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn raw_handler_partial_validation_and_decode_cleanup_are_stack_safe() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let deep = deep_value(10_000);
            for tail in [r#"{"bytes":{"value":"!"}}"#, r#"{"bytes":{"value":false}}"#] {
                let bytes = format!(
                    r#"{{"entries":[["deep",{deep},null],["bad",{tail},null]],"kind":"Within"}}"#
                );
                let error = from_slice::<WireHandlerFrame>(bytes.as_bytes()).unwrap_err();
                assert!(matches!(error, JsonInputFailure::Decode(_)));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn unseal_keeps_syntax_and_structural_failures_typed_and_dump_evidence_report_only() {
    for (body, syntax) in [(b"{".as_slice(), true), (b"false".as_slice(), false)] {
        let mut bytes = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        bytes.extend(body);
        let error = Sealed(bytes.into()).unseal().unwrap_err();
        assert_eq!(error.hints().len(), 1);
        let Reason::Protocol(failure) = error.reason else {
            panic!("protocol")
        };
        let ProtocolFailure::SeedFrame(SeedFrameFailure::Frame(FrameFailure::Json(cause))) =
            *failure
        else {
            panic!("typed frame JSON cause")
        };
        assert_eq!(matches!(cause, JsonInputFailure::Syntax(_)), syntax);
    }
}

fn deep_runtime_data(depth: usize) -> SerialValue {
    (0..depth).fold(SerialValue::Int { value: 1 }, |value, index| {
        match index % 3 {
            0 => SerialValue::Variant {
                label: "deep".into(),
                payload: Some(Box::new(value)),
            },
            1 => SerialValue::List { items: vec![value] },
            _ => SerialValue::Map {
                entries: vec![("child".into(), value)],
            },
        }
    })
}

fn serial_binding(value: SerialValue) -> SerialBinding {
    SerialBinding {
        value,
        scheme: None,
    }
}

fn unknown_native(name: &str) -> SerialValue {
    SerialValue::Ext(SerialClosure::Native(SerialNative { name: name.into() }))
}

fn captured_row(bindings: u32) -> SerialValue {
    SerialValue::Ext(SerialClosure::Thunk(SerialThunk {
        comp: Arc::new(Spanned::synthetic(CompKind::Return(crate::ir::Val::Unit))),
        env: SerialEnvSnapshot { bindings },
    }))
}

#[test]
fn runtime_binding_prefix_cleanup_handles_deep_data_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let table = vec![vec![
                ("deep".into(), serial_binding(deep_runtime_data(30_000))),
                (
                    "bad".into(),
                    serial_binding(unknown_native("missing-prefix")),
                ),
            ]];
            let error = WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table)
                .expect_err("later binding refuses");
            assert_eq!(
                error.reason,
                Reason::Protocol(Box::new(ProtocolFailure::UnknownNative(
                    "missing-prefix".into()
                )))
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn runtime_handler_prefix_cleanup_handles_later_frame_and_env_failures_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let runtime = crate::test_helper::core_runtime();
            for later_handler in [true, false] {
                let mut seed = seed();
                seed.scope_table = vec![vec![(
                    "captured".into(),
                    serial_binding(deep_runtime_data(30_000)),
                )]];
                seed.env = SerialEnvSnapshot { bindings: 0 };
                seed.context.handlers = vec![WireHandlerFrame {
                    entries: vec![
                        ("deep".into(), deep_runtime_data(30_000), None),
                        ("closure".into(), captured_row(0), None),
                    ],
                    catch_all: Some(deep_runtime_data(30_000)),
                    kind: FrameKind::Within,
                }];
                let expected = if later_handler {
                    seed.context.handlers.push(WireHandlerFrame {
                        entries: vec![("bad".into(), unknown_native("missing-handler"), None)],
                        catch_all: None,
                        kind: FrameKind::Within,
                    });
                    ProtocolFailure::UnknownNative("missing-handler".into())
                } else {
                    seed.env.bindings = u32::MAX;
                    ProtocolFailure::UnresolvedScope(u64::from(u32::MAX).into())
                };
                let Err(error) = seed.decode(&runtime) else {
                    panic!("later refusal")
                };
                assert_eq!(error.reason, Reason::Protocol(Box::new(expected)));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn runtime_unreachable_rows_release_deep_data_and_captures_on_success_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut seed = seed();
            let mut deep_row = vec![("deep".into(), serial_binding(deep_runtime_data(30_000)))];
            deep_row.extend(
                (0..12).map(|index| (format!("pad{index}"), serial_binding(SerialValue::Unit))),
            );
            seed.scope_table = vec![
                vec![],
                deep_row,
                vec![("closure".into(), serial_binding(captured_row(1)))],
            ];
            seed.env = SerialEnvSnapshot { bindings: 0 };
            seed.context.handlers.clear();
            let bequest = seed.decode(&crate::test_helper::core_runtime()).unwrap();
            assert!(bequest.image.env.get("deep").is_none());
            drop(bequest);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn runtime_duplicate_map_replacement_releases_deep_data_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let runtime = crate::test_helper::core_runtime();
            let table = vec![vec![(
                "deep".into(),
                serial_binding(deep_runtime_data(30_000)),
            )]];
            let dec = WireDecoder::for_runtime(&runtime, &table).unwrap();
            let value = SerialValue::Map {
                entries: vec![
                    (
                        "same".into(),
                        SerialValue::List {
                            items: vec![deep_runtime_data(30_000), captured_row(0)],
                        },
                    ),
                    ("same".into(), SerialValue::Int { value: 2 }),
                ],
            }
            .into_runtime(&dec)
            .unwrap();
            let crate::shell::runtime::Value::Map(map) = &value else {
                panic!("map")
            };
            assert_eq!(
                map.get("same").unwrap().as_ref(),
                &crate::shell::runtime::Value::Int(2)
            );
            value.drop_seed();

            // Discarded duplicate entries still validate in authored order.
            for (first, expected) in [
                (
                    unknown_native("discarded"),
                    ProtocolFailure::UnknownNative("discarded".into()),
                ),
                (
                    captured_row(u32::MAX),
                    ProtocolFailure::UnresolvedScope(u64::from(u32::MAX).into()),
                ),
            ] {
                let error = SerialValue::Map {
                    entries: vec![
                        ("same".into(), first),
                        ("same".into(), unknown_native("later")),
                    ],
                }
                .into_runtime(&dec)
                .unwrap_err();
                assert_eq!(error.reason, Reason::Protocol(Box::new(expected)));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn runtime_duplicate_scope_bindings_release_displaced_deep_data_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let runtime = crate::test_helper::core_runtime();
            for large in [false, true] {
                let mut row = vec![
                    ("same".into(), serial_binding(deep_runtime_data(30_000))),
                    ("same".into(), serial_binding(SerialValue::Int { value: 2 })),
                ];
                if large {
                    row.extend(
                        (0..12).map(|index| {
                            (format!("pad{index}"), serial_binding(SerialValue::Unit))
                        }),
                    );
                }
                let table = vec![row];
                let dec = WireDecoder::for_runtime(&runtime, &table).unwrap();
                let env = SerialEnvSnapshot { bindings: 0 }
                    .into_runtime(&dec)
                    .unwrap();
                let alias = SerialEnvSnapshot { bindings: 0 }
                    .into_runtime(&dec)
                    .unwrap();
                assert!(env.ptr_eq(&alias));
                assert_eq!(env.get("same"), Some(&crate::shell::runtime::Value::Int(2)));
                env.drop_seed();
                alias.drop_seed();
                drop(dec);
            }
            let table = vec![vec![
                (
                    "same".into(),
                    serial_binding(unknown_native("discarded-binding")),
                ),
                ("same".into(), serial_binding(SerialValue::Unit)),
            ]];
            let error = WireDecoder::for_runtime(&runtime, &table).unwrap_err();
            assert_eq!(
                error.reason,
                Reason::Protocol(Box::new(ProtocolFailure::UnknownNative(
                    "discarded-binding".into()
                )))
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

fn open_scheme() -> serde_json::Value {
    serde_json::to_value(crate::ty::Scheme::mono(crate::ty::Ty::var(
        crate::ty::TyVar(7),
    )))
    .unwrap()
}

#[test]
fn open_binding_scheme_is_refused_at_its_path() {
    let mut value = with_binding(envelope(), &json!("unit"));
    value["scope_table"][0][0][1]["scheme"] = open_scheme();
    let error = value_failure(&value);
    let mut path = table_path();
    path.pop();
    path.push(DataStep::Field("scheme".into()));
    assert_eq!(error.path, path);
    assert!(matches!(*error.cause, DecodeCause::OpenScheme(_)));
}

#[test]
fn open_handler_scheme_is_refused_at_its_tuple_slot() {
    let mut value = envelope();
    value["context"]["handlers"] = json!([{
        "entries": [["open", "unit", open_scheme()]], "kind": "Alias"
    }]);
    let error = value_failure(&value);
    assert_eq!(
        error.path,
        [
            DataStep::Field("context".into()),
            DataStep::Field("handlers".into()),
            DataStep::Index(0usize.into()),
            DataStep::Field("entries".into()),
            DataStep::Index(0usize.into()),
            DataStep::Index(2usize.into()),
        ]
    );
    assert!(matches!(*error.cause, DecodeCause::OpenScheme(_)));
}

/// A partially applied builtin's code: a λ over a native call.
fn native_comp() -> Arc<crate::ir::Comp> {
    Arc::new(Spanned::synthetic(CompKind::Lam {
        param: "x".into(),
        body: Arc::new(Spanned::synthetic(CompKind::Native {
            entry: crate::ir::NativeEntry(Arc::new(
                crate::shell::builtins::CORE_BUILTINS[0].clone(),
            )),
            args: Box::new([]),
        })),
    }))
}

fn native_thunk() -> SerialValue {
    SerialValue::Ext(SerialClosure::Thunk(SerialThunk {
        comp: native_comp(),
        env: SerialEnvSnapshot { bindings: 0 },
    }))
}

#[test]
fn raw_seed_native_computation_encoding_keeps_table_path_and_name() {
    let mut seed = seed();
    seed.scope_table = vec![vec![(
        "native".into(),
        SerialBinding {
            value: native_thunk(),
            scheme: None,
        },
    )]];
    let error = to_vec(&seed).unwrap_err();
    let mut path = table_path();
    path.extend([
        DataStep::Payload("ext".into()),
        DataStep::Payload("Thunk".into()),
        DataStep::Field("comp".into()),
    ]);
    assert_eq!(error.path, path);
    assert!(
        matches!(error.cause, JsonEncodeCause::NativeComputation(cause)
        if cause.name == crate::shell::builtins::CORE_BUILTINS[0].decl.name.as_ref())
    );
}

#[test]
fn raw_handler_native_computation_encoding_keeps_tuple_and_catch_all_paths() {
    let mut frame = WireHandlerFrame {
        entries: vec![("named".into(), native_thunk(), None)],
        catch_all: None,
        kind: FrameKind::Within,
    };
    let error = to_vec(&frame).unwrap_err();
    assert_eq!(
        &error.path[..3],
        [
            DataStep::Field("entries".into()),
            DataStep::Index(0usize.into()),
            DataStep::Index(1usize.into())
        ]
    );
    for (_, value, _) in std::mem::take(&mut frame.entries) {
        drop_value(value);
    }
    frame.catch_all = Some(native_thunk());
    assert_eq!(
        to_vec(&frame).unwrap_err().path[0],
        DataStep::Field("catch_all".into())
    );
}

#[test]
fn seal_classifies_only_typed_frame_encoding_failure_at_boundary() {
    let mut bequest = crate::test_helper::core_shell().bequeath(SpawnGrant::Inherit);
    let SerialValue::Ext(SerialClosure::Thunk(thunk)) = native_thunk() else {
        unreachable!()
    };
    bequest.image.env.bind(
        "unserializable".into(),
        crate::shell::runtime::Binding {
            value: crate::Value::Thunk(crate::shell::runtime::Closure::closed(
                (*thunk.comp).clone(),
            )),
            scheme: None,
        },
    );
    let Reason::Protocol(failure) = super::super::seal(&bequest).unwrap_err().reason else {
        panic!("frame boundary")
    };
    let ProtocolFailure::Unencodable(failure) = *failure else {
        panic!("encoding")
    };
    assert_eq!(failure.operation, ProtocolOperation::SealSeed);
    assert!(matches!(failure.cause, FrameEncodeFailure::Json(_)));
}

#[test]
fn raw_binding_partial_encoding_cleanup_is_stack_safe() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let deep: SerialValue = from_slice(deep_value(10_000).as_bytes()).unwrap();
            let binding = SerialBinding {
                value: SerialValue::List {
                    items: vec![deep, native_thunk()],
                },
                scheme: None,
            };
            assert!(matches!(
                to_vec(&binding).unwrap_err().cause,
                JsonEncodeCause::NativeComputation(_)
            ));
            drop(binding);
        })
        .unwrap()
        .join()
        .unwrap();
}
