use crate::data::Bytes;
use crate::fact::{DataStep, DecodeCause, DecodeFailure, ProtocolData, ProtocolFailure, Reason};
use crate::ir::{
    ArithOp, BinaryOp, CaseArm, CommandName, CommandWord, Comp, CompKind, Exec, Key, MapEntryV,
    Mentions, Module, Node, Pattern, Phrase, Prim, Redirects, StderrTarget, StdinSource, Toplevel,
    Val, ValListElem, WriteMode,
};
use crate::marshal::{self, Marshalled};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{Binding, Env, ValClos};
use crate::source::{FileId, SourceFile, SourceOrigin, Span, Spanned};
use crate::ty::{Closed, Scheme, Site, Ty, TyVar, UnitId};
use serde_json::{Value as Json, json};
use std::sync::Arc;

fn field(name: &str) -> DataStep {
    DataStep::Field(name.into())
}

fn payload(name: &str) -> DataStep {
    DataStep::Payload(name.into())
}

fn index(n: usize) -> DataStep {
    DataStep::Index(n.into())
}

fn bytes() -> Val {
    Val::Bytes(vec![0, 1, 127, 128, 255].into())
}

fn comp(item: CompKind) -> Arc<Comp> {
    Arc::new(Spanned::synthetic(item))
}

fn ret(value: Val) -> Arc<Comp> {
    comp(CompKind::Return(value))
}

fn exec(redirects: Redirects<Val>) -> Exec {
    Exec {
        head: CommandWord::External(CommandName::Path("/bin/echo".into())),
        argv: Vec::new().into(),
        redirects,
        site: None,
    }
}

fn module(phrases: Vec<Spanned<Phrase>>) -> Arc<Module> {
    Arc::new(Module {
        top: Toplevel {
            phrases,
            admits: Vec::new(),
        },
        origin: SourceOrigin::named("<linked-ir>"),
    })
}

fn closed() -> Closed {
    Closed::new(Arc::new(Scheme::mono(Ty::bytes()))).unwrap()
}

fn definition() -> Spanned<Phrase> {
    Spanned::synthetic(Phrase::Define {
        pattern: Arc::new(Pattern::Name("bytes".into())),
        comp: ret(bytes()),
        hole: (),
        schemes: vec![("bytes".into(), closed())],
    })
}

fn envelope(value: &Val) -> Json {
    let parent = crate::test_helper::core_shell()
        .marshal(&SpawnGrant::Inherit)
        .unwrap();
    let mut document: Json = serde_json::from_slice(&parent.0).unwrap();
    document["rows"] = json!([{"Env": [["ir", {
        "value": {"Closure": {"tags": [], "val": value, "env": null}},
        "scheme": null
    }]]}]);
    document["env"] = json!(0);
    document
}

fn value_path() -> Vec<DataStep> {
    vec![
        field("rows"),
        index(0),
        payload("Env"),
        index(0),
        index(1),
        field("value"),
        payload("Closure"),
        field("val"),
    ]
}

fn replace(document: &mut Json, path: &[DataStep], value: Json) {
    let target = path.iter().fold(document, |value, step| match step {
        DataStep::Field(name) | DataStep::Payload(name) => value.get_mut(name).unwrap(),
        DataStep::Index(n) => value.get_mut(usize::try_from(n.to_u64()).unwrap()).unwrap(),
    });
    *target = value;
}

fn refused(document: &Json) -> DecodeFailure {
    let parent = Marshalled(serde_json::to_vec(document).unwrap().into());
    let Err(error) = marshal::read(&crate::test_helper::core_runtime(), &parent) else {
        panic!("malformed IR must be refused by the full reader")
    };
    let Reason::Protocol(protocol) = error.reason else {
        panic!("IR admission must return a protocol failure")
    };
    let ProtocolFailure::Decode(failure) = *protocol else {
        panic!("IR admission must have exactly one protocol decode wrapper")
    };
    assert_eq!(failure.kind, ProtocolData::Parent);
    assert_eq!(failure.operation, "child");
    failure.cause
}

fn refuses_bytes(value: Val, inside: Vec<DataStep>) {
    let mut document = envelope(&value);
    let before = ret(value);
    assert_eq!(round_trip(before.clone()), before);
    let mut path = value_path();
    path.extend(inside);
    path.push(payload("Bytes"));
    replace(&mut document, &path, json!("!"));
    let mut expected = Bytes::from_base64("!").unwrap_err();
    assert!(matches!(*expected.cause, DecodeCause::Base64(_)));
    expected.path = path;
    assert_eq!(refused(&document), expected);
}

fn refuses_comp_bytes(item: CompKind, inside: Vec<DataStep>) {
    let mut path = vec![payload("Thunk"), field("item")];
    path.extend(inside);
    refuses_bytes(Val::thunk(comp(item)), path);
}

#[test]
fn malformed_byte_literals_are_typed_refusals_at_complete_value_paths() {
    refuses_bytes(bytes(), Vec::new());
    for (value, path) in [
        (
            Val::list(vec![Spanned::synthetic(bytes())]),
            vec![payload("List"), index(0), field("item")],
        ),
        (
            Val::record(vec![("a".into(), Spanned::synthetic(bytes()))]),
            vec![payload("Record"), index(0), index(1), field("item")],
        ),
        (
            Val::map(vec![("a".into(), Spanned::synthetic(bytes()))]),
            vec![payload("Map"), index(0), index(1), field("item")],
        ),
        (
            Val::Variant {
                label: "nested".into(),
                payload: Box::new(Val::list(vec![Spanned::synthetic(Val::map(vec![(
                    "a".into(),
                    Spanned::synthetic(bytes()),
                )]))])),
            },
            vec![
                payload("Variant"),
                field("payload"),
                payload("List"),
                index(0),
                field("item"),
                payload("Map"),
                index(0),
                index(1),
                field("item"),
            ],
        ),
    ] {
        refuses_bytes(value, path);
    }
}

#[test]
fn computation_fields_admit_nested_bytes_before_constructing_a_closure() {
    let b = || Spanned::synthetic(bytes());
    let unit = || ret(Val::Unit);
    let cases = vec![
        (CompKind::Return(bytes()), vec![payload("Return")]),
        (CompKind::Force(bytes()), vec![payload("Force")]),
        (
            CompKind::Lam {
                param: "x".into(),
                body: ret(bytes()),
            },
            vec![
                payload("Lam"),
                field("body"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::Bind {
                comp: ret(bytes()),
                hole: (),
                pattern: Arc::new(Pattern::Name("x".into())),
                rest: unit(),
            },
            vec![
                payload("Bind"),
                field("comp"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::Bind {
                comp: unit(),
                hole: (),
                pattern: Arc::new(Pattern::Name("x".into())),
                rest: ret(bytes()),
            },
            vec![
                payload("Bind"),
                field("rest"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::Seq {
                first: ret(bytes()),
                rest: unit(),
            },
            vec![
                payload("Seq"),
                field("first"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::Seq {
                first: unit(),
                rest: ret(bytes()),
            },
            vec![
                payload("Seq"),
                field("rest"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::App {
                head: ret(bytes()),
                args: Box::new([]),
                hole: (),
            },
            vec![
                payload("App"),
                field("head"),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::App {
                head: unit(),
                args: Box::new([b()]),
                hole: (),
            },
            vec![payload("App"), field("args"), index(0), field("item")],
        ),
        (
            CompKind::Capture(ret(bytes())),
            vec![payload("Capture"), field("item"), payload("Return")],
        ),
        (
            CompKind::Pipeline {
                stages: vec![unit(), ret(bytes())],
            },
            vec![
                payload("Pipeline"),
                field("stages"),
                index(1),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::Rec {
                group: Node::new(
                    vec![("a".into(), unit()), ("b".into(), ret(bytes()))].into_boxed_slice(),
                ),
                index: 0,
            },
            vec![
                payload("Rec"),
                field("group"),
                index(1),
                index(1),
                field("item"),
                payload("Return"),
            ],
        ),
        (
            CompKind::If {
                cond: b(),
                then: Spanned::synthetic(Val::Unit),
                else_: Spanned::synthetic(Val::Unit),
            },
            vec![payload("If"), field("cond"), field("item")],
        ),
        (
            CompKind::If {
                cond: Spanned::synthetic(Val::Unit),
                then: b(),
                else_: Spanned::synthetic(Val::Unit),
            },
            vec![payload("If"), field("then"), field("item")],
        ),
        (
            CompKind::If {
                cond: Spanned::synthetic(Val::Unit),
                then: Spanned::synthetic(Val::Unit),
                else_: b(),
            },
            vec![payload("If"), field("else_"), field("item")],
        ),
        (
            CompKind::Case {
                scrutinee: b(),
                arms: Vec::new(),
            },
            vec![payload("Case"), field("scrutinee"), field("item")],
        ),
        (
            CompKind::Case {
                scrutinee: Spanned::synthetic(Val::Unit),
                arms: vec![CaseArm {
                    tag: Spanned::synthetic("a".into()),
                    body: b(),
                }],
            },
            vec![
                payload("Case"),
                field("arms"),
                index(0),
                field("body"),
                field("item"),
            ],
        ),
        (
            CompKind::Try {
                body: bytes(),
                handler: Val::Unit,
            },
            vec![payload("Try"), field("body")],
        ),
        (
            CompKind::Try {
                body: Val::Unit,
                handler: bytes(),
            },
            vec![payload("Try"), field("handler")],
        ),
        (
            CompKind::Guard {
                acquire: bytes(),
                body: Val::Unit,
                release: Val::Unit,
            },
            vec![payload("Guard"), field("acquire")],
        ),
        (
            CompKind::Guard {
                acquire: Val::Unit,
                body: bytes(),
                release: Val::Unit,
            },
            vec![payload("Guard"), field("body")],
        ),
        (
            CompKind::Guard {
                acquire: Val::Unit,
                body: Val::Unit,
                release: bytes(),
            },
            vec![payload("Guard"), field("release")],
        ),
        (
            CompKind::Within {
                opts: Node::new(vec![("a".into(), b())].into_boxed_slice()),
                body: Val::Unit,
            },
            vec![
                payload("Within"),
                field("opts"),
                index(0),
                index(1),
                field("item"),
            ],
        ),
        (
            CompKind::Within {
                opts: Node::new(Box::new([])),
                body: bytes(),
            },
            vec![payload("Within"), field("body")],
        ),
        (
            CompKind::Grant {
                caps: Node::new(vec![("a".into(), b())].into_boxed_slice()),
                body: Val::Unit,
            },
            vec![
                payload("Grant"),
                field("caps"),
                index(0),
                index(1),
                field("item"),
            ],
        ),
        (
            CompKind::Grant {
                caps: Node::new(Box::new([])),
                body: bytes(),
            },
            vec![payload("Grant"), field("body")],
        ),
        (
            CompKind::Spawn {
                title: Some(b()),
                body: Val::Unit,
            },
            vec![payload("Spawn"), field("title"), field("item")],
        ),
        (
            CompKind::Spawn {
                title: None,
                body: bytes(),
            },
            vec![payload("Spawn"), field("body")],
        ),
        (
            CompKind::Audit { body: bytes() },
            vec![payload("Audit"), field("body")],
        ),
        (
            CompKind::Gather { body: bytes() },
            vec![payload("Gather"), field("body")],
        ),
        (
            CompKind::Redirect {
                body: ret(bytes()),
                redirects: Redirects::default(),
            },
            vec![
                payload("Redirect"),
                field("body"),
                field("item"),
                payload("Return"),
            ],
        ),
    ];
    for (item, path) in cases {
        refuses_comp_bytes(item, path);
    }
}

#[test]
fn primitives_keep_enum_payloads_and_real_operand_indices_in_refusal_paths() {
    let b = || Spanned::synthetic(bytes());
    let cases = vec![
        (
            Prim::Binary(BinaryOp::Arith(ArithOp::Add), bytes(), Val::Unit),
            vec![payload("Binary"), index(1)],
        ),
        (
            Prim::Binary(BinaryOp::Arith(ArithOp::Add), Val::Unit, bytes()),
            vec![payload("Binary"), index(2)],
        ),
        (Prim::Negate(bytes()), vec![payload("Negate")]),
        (Prim::Not(bytes()), vec![payload("Not")]),
        (Prim::Render(bytes()), vec![payload("Render")]),
        (Prim::Decode(bytes()), vec![payload("Decode")]),
        (
            Prim::Concat(Box::new([Val::Unit, bytes()])),
            vec![payload("Concat"), index(1)],
        ),
        (
            Prim::Index {
                target: bytes(),
                key: Spanned::synthetic(Key::Position(0)),
            },
            vec![payload("Index"), field("target")],
        ),
        (
            Prim::Index {
                target: Val::Unit,
                key: Spanned::synthetic(Key::Data(bytes())),
            },
            vec![
                payload("Index"),
                field("key"),
                field("item"),
                payload("Data"),
            ],
        ),
        (
            Prim::Update {
                base: b(),
                fields: Box::new([]),
            },
            vec![payload("Update"), field("base"), field("item")],
        ),
        (
            Prim::Update {
                base: Spanned::synthetic(Val::Unit),
                fields: Box::new([("a".into(), b())]),
            },
            vec![
                payload("Update"),
                field("fields"),
                index(0),
                index(1),
                field("item"),
            ],
        ),
        (
            Prim::List(Box::new([ValListElem::Single(b())])),
            vec![payload("List"), index(0), payload("Single"), field("item")],
        ),
        (
            Prim::List(Box::new([ValListElem::Spread(b())])),
            vec![payload("List"), index(0), payload("Spread"), field("item")],
        ),
        (
            Prim::Map(Box::new([MapEntryV::Entry(
                b(),
                Spanned::synthetic(Val::Unit),
            )])),
            vec![
                payload("Map"),
                index(0),
                payload("Entry"),
                index(0),
                field("item"),
            ],
        ),
        (
            Prim::Map(Box::new([MapEntryV::Entry(
                Spanned::synthetic(Val::Unit),
                b(),
            )])),
            vec![
                payload("Map"),
                index(0),
                payload("Entry"),
                index(1),
                field("item"),
            ],
        ),
        (
            Prim::Map(Box::new([MapEntryV::Spread(b())])),
            vec![payload("Map"), index(0), payload("Spread"), field("item")],
        ),
    ];
    for (prim, inside) in cases {
        let mut path = vec![payload("Prim")];
        path.extend(inside);
        refuses_comp_bytes(CompKind::Prim(prim), path);
    }
    for (slot, tag) in [
        (ValListElem::Single(b()), "Single"),
        (ValListElem::Spread(b()), "Spread"),
    ] {
        let mut call = exec(Redirects::default());
        call.argv = vec![slot].into();
        refuses_comp_bytes(
            CompKind::Exec(call),
            vec![
                payload("Exec"),
                field("argv"),
                index(0),
                payload(tag),
                field("item"),
            ],
        );
    }
}

#[test]
fn redirects_admit_both_owners_and_preserve_nested_enum_tuple_paths() {
    let cases = [
        (
            Redirects {
                stdin: Some(StdinSource::File(bytes())),
                ..Redirects::default()
            },
            vec![field("stdin"), payload("File")],
        ),
        (
            Redirects {
                stdin: Some(StdinSource::Here(bytes())),
                ..Redirects::default()
            },
            vec![field("stdin"), payload("Here")],
        ),
        (
            Redirects {
                stdout: Some((WriteMode::Write, bytes())),
                ..Redirects::default()
            },
            vec![field("stdout"), index(1)],
        ),
        (
            Redirects {
                stderr: Some(StderrTarget::File(WriteMode::Append, bytes())),
                ..Redirects::default()
            },
            vec![field("stderr"), payload("File"), index(1)],
        ),
    ];
    for (redirects, inside) in cases {
        for (tag, item) in [
            ("Exec", CompKind::Exec(exec(redirects.clone()))),
            (
                "Redirect",
                CompKind::Redirect {
                    body: ret(Val::Unit),
                    redirects: redirects.clone(),
                },
            ),
        ] {
            let mut path = vec![payload(tag), field("redirects")];
            path.extend(inside.clone());
            refuses_comp_bytes(item, path);
        }
    }
}

fn use_path() -> Vec<DataStep> {
    vec![
        payload("Thunk"),
        field("item"),
        payload("Use"),
        field("top"),
        field("phrases"),
        index(0),
        field("item"),
    ]
}

#[test]
fn open_schemes_in_linked_and_recursively_linked_modules_are_typed_refusals() {
    let scheme = Arc::new(Scheme::mono(Ty::var(TyVar(7))));
    let cause = DecodeCause::OpenScheme(Closed::new(Arc::clone(&scheme)).unwrap_err());
    for nested in [false, true] {
        let linked = comp(CompKind::Use(module(vec![definition()])));
        let value = if nested {
            Val::thunk(comp(CompKind::Use(module(vec![Spanned::synthetic(
                Phrase::Run(linked),
            )]))))
        } else {
            Val::thunk(linked)
        };
        let mut path = value_path();
        path.extend(use_path());
        if nested {
            path.extend([
                payload("Run"),
                field("item"),
                payload("Use"),
                field("top"),
                field("phrases"),
                index(0),
                field("item"),
            ]);
        }
        path.extend([payload("Define"), field("schemes"), index(0), index(1)]);
        let mut document = envelope(&value);
        replace(&mut document, &path, serde_json::to_value(&scheme).unwrap());
        assert_eq!(
            refused(&document),
            DecodeFailure {
                path,
                cause: Box::new(cause.clone())
            }
        );
    }
}

#[test]
fn linked_module_definition_and_nested_run_bodies_admit_bytes_recursively() {
    let mut path = use_path();
    path.extend([
        payload("Define"),
        field("comp"),
        field("item"),
        payload("Return"),
    ]);
    refuses_bytes(
        Val::thunk(comp(CompKind::Use(module(vec![definition()])))),
        path,
    );

    let inner = comp(CompKind::Use(module(vec![Spanned::synthetic(
        Phrase::Run(ret(bytes())),
    )])));
    let mut path = use_path();
    path.extend([
        payload("Run"),
        field("item"),
        payload("Use"),
        field("top"),
        field("phrases"),
        index(0),
        field("item"),
        payload("Run"),
        field("item"),
        payload("Return"),
    ]);
    refuses_bytes(
        Val::thunk(comp(CompKind::Use(module(vec![Spanned::synthetic(
            Phrase::Run(inner),
        )])))),
        path,
    );
}

fn round_trip(body: Arc<Comp>) -> Arc<Comp> {
    let mut names = Vec::new();
    body.mentions(&mut names);
    names.sort_unstable();
    names.dedup();
    let env = Env::from_bindings(
        names
            .into_iter()
            .map(|name| {
                (
                    name.clone(),
                    Binding {
                        value: ValClos::UNIT,
                        scheme: None,
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    let mut shell = crate::test_helper::core_shell();
    shell.env.bind(
        "ir".into(),
        Binding {
            value: ValClos::thunk(body, &env),
            scheme: Some(closed()),
        },
    );
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let (image, grant) = marshal::read(&shell.session.runtime, &parent).unwrap();
    assert!(matches!(grant, SpawnGrant::Inherit));
    let binding = image.env.binding("ir").unwrap();
    assert_eq!(binding.scheme, Some(closed()));
    binding.value.block().unwrap().0.clone()
}

#[test]
fn every_serializable_ir_form_survives_runtime_write_raw_decode_and_admission() {
    for body in crate::ir::test_comps()
        .into_iter()
        .filter(|body| !matches!(body.item, CompKind::Builtin { .. }))
    {
        assert_eq!(round_trip(body.clone()), body);
    }
}

#[test]
fn bytes_closed_schemes_spans_sites_units_origins_and_module_admits_survive() {
    let span = Span::new(FileId(73), 11, 29);
    let site = Arc::new(Site {
        nodes: vec![crate::ty::Node {
            shape: crate::ty::Shape::Bytes,
            witness: Some(span),
        }]
        .into(),
        root: 0,
        unit: UnitId::fresh(),
    });
    let mut call = exec(Redirects {
        stdin: Some(StdinSource::Here(bytes())),
        stdout: Some((WriteMode::Stream, bytes())),
        stderr: Some(StderrTarget::Stdout),
    });
    call.argv = vec![ValListElem::Single(Spanned::new(span, bytes()))].into();
    call.site = Some(site.clone());
    let linked = Arc::new(Module {
        top: Toplevel {
            phrases: vec![
                Spanned::new(
                    span,
                    Phrase::Define {
                        pattern: Arc::new(Pattern::Name("bytes".into())),
                        comp: Arc::new(Spanned::new(
                            span,
                            CompKind::Return(Val::record(vec![(
                                "payload".into(),
                                Spanned::new(span, bytes()),
                            )])),
                        )),
                        hole: (),
                        schemes: vec![("bytes".into(), closed())],
                    },
                ),
                Spanned::new(
                    span,
                    Phrase::Run(Arc::new(Spanned::new(span, CompKind::Exec(call)))),
                ),
                Spanned::new(
                    span,
                    Phrase::Run(comp(CompKind::Use(module(vec![definition()])))),
                ),
            ],
            admits: vec![("stored".into(), site)],
        },
        origin: SourceOrigin::File(SourceFile::new("/linked/module.ral").unwrap()),
    });
    let before = Arc::new(Spanned::new(span, CompKind::Use(linked)));
    assert_eq!(round_trip(before.clone()), before);
}

#[test]
fn recursive_ir_admission_grows_its_json_stack_at_bounded_depth() {
    // Raw IR drop is still recursive (F1); this is not an arbitrary-depth drop test.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let mut body = ret(bytes());
            for _ in 0..512 {
                body = comp(CompKind::Capture(body));
            }
            let mut shell = crate::test_helper::core_shell();
            shell.env.bind(
                "ir".into(),
                Binding {
                    value: ValClos::thunk(body, &Env::EMPTY),
                    scheme: None,
                },
            );
            let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
            // Leave room for bounded raw-IR drop, independently of admission's guard.
            let (image, _) = stacker::grow(2 * 1024 * 1024, || {
                marshal::read(&shell.session.runtime, &parent).unwrap()
            });
            let mut after = image.env.get("ir").unwrap().block().unwrap().0.as_ref();
            let mut depth = 0;
            while let CompKind::Capture(inner) = &after.item {
                depth += 1;
                after = inner;
            }
            assert_eq!(depth, 512);
            assert_eq!(after, ret(bytes()).as_ref());
        })
        .unwrap()
        .join()
        .expect("bounded IR admission grows its stack and drops on an adequate stack");
}
