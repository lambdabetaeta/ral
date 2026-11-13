use crate::data::json::{JsonDocument, JsonFormat, decode_structural, serialize_structural};
use crate::fact::{
    DataStep, DecodeCause, DecodeFailure, EvalFailure, InvalidResidual, ProtocolData,
    ProtocolFailure, Reason, ResidualDefect, RowDefect, RowKind, UnresolvedRow,
};
use crate::ir::{Comp, CompKind, Val};
use crate::marshal::wire::{self, Row, Value, Wire};
use crate::marshal::{self, Marshalled};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{
    Break, Error, Mooring, Settled, Shell, ValClos, View, block_over, captured, outcome_value,
};
use crate::source::Spanned;
use std::sync::Arc;

fn no_base(
    _: &str,
    _: &crate::authority::grant::freeze::FreezeCtx,
) -> Result<crate::authority::policy::GrantLayer, Error> {
    panic!("an inherited grant does not consult the narrower")
}

fn child(parent: &Shell) -> Shell {
    Shell::child_of(
        Arc::new(crate::test_helper::core_runtime()),
        &parent.marshal(&SpawnGrant::Inherit).unwrap(),
        no_base,
    )
    .unwrap()
}

fn builtin(shell: &Shell, name: &str) -> ValClos {
    ValClos::builtin(Arc::new(shell.lookup_builtin(name).unwrap()))
}

fn apply(shell: &mut Shell, value: &ValClos, args: Vec<ValClos>) -> Settled<ValClos> {
    crate::shell::evaluator::apply(value, args, &Mooring::adrift(), shell)
}

fn error(result: Settled<ValClos>) -> Error {
    let Break::Error(error) = result.unwrap_err() else {
        panic!("expected a typed error, not an escape")
    };
    error
}

fn raw() -> Wire {
    let parent = crate::test_helper::core_shell()
        .marshal(&SpawnGrant::Inherit)
        .unwrap();
    let document = JsonDocument::parse(&parent.0).unwrap();
    decode_structural(document.root()).unwrap()
}

fn encoded(wire: &Wire) -> Marshalled {
    Marshalled(serialize_structural(wire, JsonFormat::Compact).into())
}

fn refused(parent: &Marshalled) -> DecodeFailure {
    let Err(error) = marshal::read(&crate::test_helper::core_runtime(), parent) else {
        panic!("the malformed parent must be refused")
    };
    let Reason::Protocol(failure) = error.reason else {
        panic!("a refused parent must be a protocol failure")
    };
    let ProtocolFailure::Decode(failure) = *failure else {
        panic!("a refused parent must be wrapped exactly once as Decode")
    };
    assert_eq!(failure.kind, ProtocolData::Parent);
    assert_eq!(failure.operation, "child");
    failure.cause
}

fn binding(value: Value) -> wire::Binding {
    wire::Binding {
        value,
        scheme: None,
    }
}

fn unit() -> Value {
    Value::Closure {
        tags: Vec::new(),
        val: Val::Unit,
        env: None,
    }
}

fn binding_path(row: usize, entry: usize) -> Vec<DataStep> {
    vec![
        DataStep::Field("rows".into()),
        DataStep::Index(row.into()),
        DataStep::Payload("Env".into()),
        DataStep::Index(entry.into()),
        DataStep::Index(1usize.into()),
        DataStep::Field("value".into()),
    ]
}

#[test]
fn live_handles_cross_withheld_in_every_value_position() {
    use crate::handle::{Handle, idle_handle};
    use crate::test_access::bind_untyped;

    let mut parent = crate::test_helper::core_shell();
    let live = idle_handle();
    assert!(matches!(live.view(), View::Handle(Handle::Live(_))));
    bind_untyped(&mut parent, "live", live.clone());
    let capture = block_over(&parent.env);
    bind_untyped(&mut parent, "capture", capture.clone());
    for (name, list) in [
        (
            "literal",
            ValClos::closure(
                Vec::new(),
                Val::list(vec![
                    Spanned::synthetic(Val::Variable("live".into())),
                    Spanned::synthetic(Val::Int(7)),
                    Spanned::synthetic(Val::Variable("live".into())),
                ]),
                parent.env.clone(),
            )
            .unwrap(),
        ),
        (
            "built",
            ValClos::list(vec![live.clone(), ValClos::int(7), live.clone()]),
        ),
    ] {
        bind_untyped(&mut parent, name, list);
    }
    bind_untyped(
        &mut parent,
        "map",
        ValClos::map(vec![("worker".into(), live.clone())]),
    );
    let has = builtin(&parent, "has");
    let residual = apply(&mut parent, &has, vec![live.clone()]).unwrap();
    bind_untyped(&mut parent, "residual", residual);
    bind_untyped(
        &mut parent,
        "tagged",
        ValClos::variant("worker", Some(live)),
    );
    // An arm is a thunk, so a handle reaches a frame only through a capture.
    parent.context.handlers.push(
        vec![crate::shell::runtime::HandlerEntry::ral_per_name(
            "captured".into(),
            capture.clone(),
        )],
        Some(capture),
    );

    let mut child = child(&parent);
    let mut handles = vec![
        child.env.get("live").unwrap().clone(),
        captured(child.env.get("capture"))
            .get("live")
            .unwrap()
            .clone(),
        captured(child.env.get("residual"))
            .get("%arg0")
            .unwrap()
            .clone(),
    ];
    for name in ["literal", "built"] {
        let View::List(items) = child.env.get(name).unwrap().view() else {
            panic!("{name} remains a list")
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items.get(1).unwrap().as_ref(), &ValClos::int(7));
        handles.extend([0, 2].map(|index| items.get(index).unwrap().into_owned()));
    }
    let View::Map(map) = child.env.get("map").unwrap().view() else {
        panic!("the map remains a map")
    };
    handles.push(map.get("worker").unwrap().into_owned());
    let (tags, payload) = child.env.get("tagged").unwrap().peel_variants();
    assert_eq!(
        tags.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        ["worker"]
    );
    handles.push(payload.into_owned());
    let frame = child.context.handlers.iter().last().unwrap();
    for arm in [&frame.entries[0].thunk, frame.catch_all.as_ref().unwrap()] {
        handles.push(captured(Some(arm)).get("live").unwrap().clone());
    }

    let await_builtin = builtin(&child, "await");
    for handle in handles {
        assert!(
            matches!(handle.view(), View::Handle(Handle::Withheld(title)) if title.as_ref() == "<test>")
        );
        let failure = error(apply(&mut child, &await_builtin, vec![handle]));
        assert_eq!(
            failure.reason,
            Reason::Evaluation(Box::new(EvalFailure::WithheldHandle("<test>".into())))
        );
        assert!(failure.describe().contains("<test>"));
    }
}

#[test]
fn bare_zero_supplied_and_partial_builtins_saturate_in_the_child() {
    use crate::test_access::bind_untyped;

    let mut parent = crate::test_helper::core_shell();
    let has = builtin(&parent, "has");
    let map = ValClos::map(vec![("present".into(), ValClos::int(7))]);
    let zero = apply(&mut parent, &has, Vec::new()).unwrap();
    let partial = apply(&mut parent, &has, vec![map.clone()]).unwrap();
    assert!(matches!(zero.view(), View::Thunk { .. }));
    assert_eq!(zero.residual().unwrap().1, 2);
    assert_eq!(partial.residual().unwrap().1, 1);
    bind_untyped(&mut parent, "bare", has);
    bind_untyped(&mut parent, "zero", zero);
    bind_untyped(&mut parent, "partial", partial.clone());
    bind_untyped(&mut parent, "tagged", outcome_value(Ok(partial)));

    let mut child = child(&parent);
    assert!(matches!(
        child.env.get("bare").unwrap().view(),
        View::Builtin(_)
    ));
    for name in ["bare", "zero", "partial", "tagged"] {
        let value = child.env.get(name).unwrap().clone();
        let value = if name == "tagged" {
            let (tags, payload) = value.peel_variants();
            assert_eq!(tags.iter().map(AsRef::as_ref).collect::<Vec<_>>(), ["ok"]);
            payload.into_owned()
        } else {
            value
        };
        for (key, expected) in [("present", true), ("absent", false)] {
            let mut args = Vec::new();
            if matches!(name, "bare" | "zero") {
                args.push(map.clone());
            }
            args.push(ValClos::string(key));
            assert_eq!(
                apply(&mut child, &value, args).unwrap(),
                ValClos::bool(expected),
                "{name}"
            );
        }
    }
}

#[test]
fn unknown_builtins_are_refused_at_the_value_that_names_them() {
    let runtime = crate::test_helper::core_runtime();
    assert!(runtime.builtins.get("echo").is_some());
    assert!(runtime.sig.native("echo").is_none());
    for (name, residual) in [
        ("not-installed", false),
        ("not-installed", true),
        ("echo", false),
        ("echo", true),
    ] {
        let mut wire = raw();
        wire.rows = vec![
            Row::Env(Vec::new()),
            Row::Env(vec![(
                "unknown".into(),
                binding(if residual {
                    Value::Residual {
                        builtin: name.into(),
                        env: 0,
                        supplied: 0,
                    }
                } else {
                    Value::Builtin(name.into())
                }),
            )]),
        ];
        wire.env = Some(1);
        let mut path = binding_path(1, 0);
        path.push(DataStep::Payload(
            if residual { "Residual" } else { "Builtin" }.into(),
        ));
        if residual {
            path.push(DataStep::Field("builtin".into()));
        }
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path,
                cause: Box::new(DecodeCause::UnknownBuiltin(name.into()))
            }
        );
    }
}

#[test]
fn residuals_require_fewer_than_arity_arguments_and_the_exact_eta_environment() {
    let entries = |supplied: usize| {
        let mut entries = vec![("%builtin".into(), binding(Value::Builtin("has".into())))];
        entries.extend((0..supplied).map(|index| (format!("%arg{index}"), binding(unit()))));
        entries
    };
    let mut extra = entries(1);
    extra.push(("extra".into(), binding(unit())));
    let cases = [
        (
            0,
            Vec::new(),
            ResidualDefect::EnvironmentSize(0usize.into()),
        ),
        (
            0,
            vec![("%builtin".into(), binding(unit()))],
            ResidualDefect::BuiltinForeign,
        ),
        (
            0,
            vec![("%builtin".into(), binding(Value::Builtin("keys".into())))],
            ResidualDefect::BuiltinForeign,
        ),
        (
            0,
            vec![("other".into(), binding(unit()))],
            ResidualDefect::BuiltinUnbound,
        ),
        (
            0,
            vec![
                ("%builtin".into(), binding(Value::Builtin("has".into()))),
                ("%arg0".into(), binding(unit())),
            ],
            ResidualDefect::EnvironmentSize(2usize.into()),
        ),
        (
            1,
            entries(0),
            ResidualDefect::EnvironmentSize(1usize.into()),
        ),
        (
            1,
            vec![
                ("%builtin".into(), binding(Value::Builtin("has".into()))),
                ("%arg1".into(), binding(unit())),
            ],
            ResidualDefect::ArgumentUnbound("%arg0".into()),
        ),
        (1, extra, ResidualDefect::EnvironmentSize(3usize.into())),
        (2, entries(2), ResidualDefect::Saturated),
        (3, entries(3), ResidualDefect::Saturated),
    ];
    for (supplied, env, defect) in cases {
        let mut wire = raw();
        wire.rows = vec![
            Row::Env(env),
            Row::Env(vec![(
                "residual".into(),
                binding(Value::Residual {
                    builtin: "has".into(),
                    env: 0,
                    supplied,
                }),
            )]),
        ];
        wire.env = Some(1);
        let mut path = binding_path(1, 0);
        path.push(DataStep::Payload("Residual".into()));
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path,
                cause: Box::new(DecodeCause::InvalidResidual(InvalidResidual {
                    name: "has".into(),
                    arity: 2usize.into(),
                    supplied: u64::from(supplied).into(),
                    defect,
                })),
            },
            "supplied={supplied}"
        );
    }
}

#[test]
fn row_references_refuse_forward_out_of_range_and_wrong_kinds_at_their_paths() {
    for built in [false, true] {
        for row in [1, 2, u32::MAX, 0] {
            let mut wire = raw();
            wire.rows = vec![
                if built {
                    Row::Env(Vec::new())
                } else {
                    Row::List(Vec::new())
                },
                Row::Env(vec![(
                    "reference".into(),
                    binding(if built {
                        Value::Built(row)
                    } else {
                        Value::Closure {
                            tags: Vec::new(),
                            val: Val::Unit,
                            env: Some(row),
                        }
                    }),
                )]),
                Row::Env(Vec::new()),
            ];
            wire.env = Some(1);
            let mut path = binding_path(1, 0);
            path.push(DataStep::Payload(
                if built { "Built" } else { "Closure" }.into(),
            ));
            if !built {
                path.push(DataStep::Field("env".into()));
            }
            assert_eq!(
                refused(&encoded(&wire)),
                DecodeFailure {
                    path,
                    cause: Box::new(DecodeCause::UnresolvedRow(UnresolvedRow {
                        row: u64::from(row).into(),
                        wanted: if built {
                            RowKind::Built
                        } else {
                            RowKind::Environment
                        },
                        defect: match row {
                            0 if built => RowDefect::WrongKind(RowKind::Environment),
                            0 => RowDefect::WrongKind(RowKind::Built),
                            u32::MAX => RowDefect::Absent(3usize.into()),
                            _ => RowDefect::Forward(1usize.into()),
                        },
                    })),
                }
            );
        }
    }
    for row in [0, 1, u32::MAX] {
        let mut wire = raw();
        wire.rows = vec![Row::List(Vec::new())];
        wire.env = Some(row);
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path: vec![DataStep::Field("env".into())],
                cause: Box::new(DecodeCause::UnresolvedRow(UnresolvedRow {
                    row: u64::from(row).into(),
                    wanted: RowKind::Environment,
                    defect: match row {
                        0 => RowDefect::WrongKind(RowKind::Built),
                        _ => RowDefect::Absent(1usize.into()),
                    },
                })),
            }
        );
    }
}

#[test]
fn residual_environment_references_obey_the_same_reference_law() {
    for row in [0, 1, 2, u32::MAX] {
        let mut wire = raw();
        wire.rows = vec![
            Row::Map(Vec::new()),
            Row::Env(vec![(
                "residual".into(),
                binding(Value::Residual {
                    builtin: "has".into(),
                    env: row,
                    supplied: 0,
                }),
            )]),
        ];
        wire.env = Some(1);
        let mut path = binding_path(1, 0);
        path.extend([
            DataStep::Payload("Residual".into()),
            DataStep::Field("env".into()),
        ]);
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path,
                cause: Box::new(DecodeCause::UnresolvedRow(UnresolvedRow {
                    row: u64::from(row).into(),
                    wanted: RowKind::Environment,
                    defect: match row {
                        0 => RowDefect::WrongKind(RowKind::Built),
                        1 => RowDefect::Forward(1usize.into()),
                        _ => RowDefect::Absent(2usize.into()),
                    },
                })),
            }
        );
    }
}

#[test]
fn built_collection_rows_obey_the_same_reference_law() {
    for map in [false, true] {
        for row in [0, 1, 2, u32::MAX] {
            let mut wire = raw();
            let value = Value::Built(row);
            wire.rows = vec![
                Row::Env(Vec::new()),
                if map {
                    Row::Map(vec![("key".into(), value)])
                } else {
                    Row::List(vec![value])
                },
            ];
            wire.env = None;
            let mut path = vec![
                DataStep::Field("rows".into()),
                DataStep::Index(1usize.into()),
                DataStep::Payload(if map { "Map" } else { "List" }.into()),
                DataStep::Index(0usize.into()),
            ];
            if map {
                path.push(DataStep::Index(1usize.into()));
            }
            path.push(DataStep::Payload("Built".into()));
            assert_eq!(
                refused(&encoded(&wire)),
                DecodeFailure {
                    path,
                    cause: Box::new(DecodeCause::UnresolvedRow(UnresolvedRow {
                        row: u64::from(row).into(),
                        wanted: RowKind::Built,
                        defect: match row {
                            0 => RowDefect::WrongKind(RowKind::Environment),
                            1 => RowDefect::Forward(1usize.into()),
                            _ => RowDefect::Absent(2usize.into()),
                        },
                    }))
                }
            );
        }
    }
}

#[test]
fn duplicate_environment_names_and_map_keys_are_refused_at_the_duplicate() {
    for map in [false, true] {
        let mut wire = raw();
        wire.rows = vec![if map {
            Row::Map(vec![("same".into(), unit()), ("same".into(), unit())])
        } else {
            Row::Env(vec![
                ("same".into(), binding(unit())),
                ("same".into(), binding(unit())),
            ])
        }];
        wire.env = None;
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path: vec![
                    DataStep::Field("rows".into()),
                    DataStep::Index(0usize.into()),
                    DataStep::Payload(if map { "Map" } else { "Env" }.into()),
                    DataStep::Index(1usize.into()),
                    DataStep::Index(0usize.into()),
                ],
                cause: Box::new(DecodeCause::DuplicateField("same".into())),
            }
        );
    }
}

#[test]
fn open_closures_and_schemes_are_refused_at_their_binding_paths() {
    let mut wire = raw();
    wire.env = Some(0);
    for (tags, val) in [
        (Vec::new(), Val::Variable("missing".into())),
        (vec!["tagged".into()], Val::Variable("missing".into())),
        (
            Vec::new(),
            Val::list(vec![Spanned::synthetic(Val::Variable("missing".into()))]),
        ),
    ] {
        wire.rows = vec![Row::Env(vec![(
            "open".into(),
            binding(Value::Closure {
                tags,
                val,
                env: None,
            }),
        )])];
        let mut path = binding_path(0, 0);
        path.push(DataStep::Payload("Closure".into()));
        assert_eq!(
            refused(&encoded(&wire)),
            DecodeFailure {
                path,
                cause: Box::new(DecodeCause::OpenClosure("missing".into()))
            }
        );
    }

    let scheme = Arc::new(crate::ty::Scheme::mono(crate::ty::Ty::var(
        crate::ty::TyVar(7),
    )));
    let open = crate::ty::Closed::new(Arc::clone(&scheme)).unwrap_err();
    wire.rows = vec![Row::Env(vec![(
        "open".into(),
        wire::Binding {
            value: unit(),
            scheme: Some(scheme),
        },
    )])];
    let mut path = binding_path(0, 0);
    *path.last_mut().unwrap() = DataStep::Field("scheme".into());
    assert_eq!(
        refused(&encoded(&wire)),
        DecodeFailure {
            path,
            cause: Box::new(DecodeCause::OpenScheme(open))
        }
    );
}

#[test]
fn malformed_environment_rows_are_structural_failures_at_the_row_payload() {
    use crate::fact::{JsonShape, JsonShapeFailure};

    let mut json: serde_json::Value = serde_json::from_slice(&encoded(&raw()).0).unwrap();
    json["rows"] = serde_json::json!([{"Env": null}]);
    let failure = refused(&Marshalled(serde_json::to_vec(&json).unwrap().into()));
    assert_eq!(
        failure,
        DecodeFailure {
            path: vec![
                DataStep::Field("rows".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("Env".into())
            ],
            cause: Box::new(DecodeCause::JsonShape(JsonShapeFailure {
                expected: vec![JsonShape::Array],
                found: JsonShape::Null
            })),
        }
    );
}

#[test]
fn caught_failure_witnesses_reraise_the_typed_reason_including_under_err() {
    use crate::test_access::bind_untyped;

    let mut parent = crate::test_helper::core_shell();
    let await_builtin = builtin(&parent, "await");
    let caught = error(apply(
        &mut parent,
        &await_builtin,
        vec![ValClos::withheld("ancestor worker")],
    ));
    let fact = caught.fact(&parent);
    bind_untyped(&mut parent, "caught", ValClos::failure(fact.clone()));
    bind_untyped(&mut parent, "outcome", outcome_value(Err(fact.clone())));
    let mut child = child(&parent);
    let raise = builtin(&child, "raise");
    for name in ["caught", "outcome"] {
        let value = child.env.get(name).unwrap().clone();
        let value = if name == "outcome" {
            let (tags, payload) = value.peel_variants();
            assert_eq!(tags.iter().map(AsRef::as_ref).collect::<Vec<_>>(), ["err"]);
            payload.into_owned()
        } else {
            value
        };
        let View::Map(map) = value.view() else {
            panic!("a caught failure remains a map")
        };
        assert_eq!(map.witness(), Some(&fact));
        let raised = error(apply(&mut child, &raise, vec![value]));
        assert_eq!(raised.reason, caught.reason);
        assert_eq!(raised.fact(&child), fact);
    }
}

#[test]
fn builtin_delta_is_skipped_by_serde_in_both_directions() {
    let comp: Comp = Spanned::synthetic(CompKind::Builtin {
        builtin: "%builtin".into(),
        args: vec!["%arg0".into()].into_boxed_slice(),
    });
    assert!(serde_json::to_vec(&comp).is_err());
    let bytes = br#"{"span":null,"item":{"Builtin":{"builtin":"%builtin","args":["%arg0"]}}}"#;
    assert!(serde_json::from_slice::<Comp>(bytes).is_err());
    let document = JsonDocument::parse(bytes).unwrap();
    let failure = decode_structural::<Comp>(document.root()).unwrap_err();
    assert_eq!(failure.path, [DataStep::Field("item".into())]);
    assert!(
        matches!(*failure.cause, DecodeCause::UnknownTag(ref unknown) if unknown.tag == "Builtin")
    );
}

#[test]
fn a_document_nested_past_the_limit_is_refused_before_it_is_decoded() {
    let nested = |levels: usize| {
        Marshalled(
            format!("{}{}", "[".repeat(levels), "]".repeat(levels))
                .into_bytes()
                .into(),
        )
    };
    let limit = marshal::read::NESTING_LIMIT;
    assert_ne!(
        *refused(&nested(limit)).cause,
        DecodeCause::TooDeep(limit.into())
    );
    assert_eq!(
        refused(&nested(limit + 1)),
        DecodeFailure::new(DecodeCause::TooDeep(limit.into()))
    );
}

#[test]
fn a_failure_that_is_no_error_fact_is_refused_at_its_value() {
    use crate::data::FOValue;
    use crate::data::json::{RawDatum, RawFOValue};
    let mut wire = raw();
    wire.rows = vec![Row::Env(vec![(
        "failure".into(),
        binding(Value::Failure(RawDatum::new(RawFOValue::from_value(
            FOValue::Unit,
        )))),
    )])];
    wire.env = Some(0);
    let error = refused(&encoded(&wire));
    assert_eq!(
        error.path,
        [
            binding_path(0, 0),
            vec![DataStep::Payload("Failure".into())]
        ]
        .concat()
    );
    assert!(matches!(*error.cause, DecodeCause::Shape(_)));
}
